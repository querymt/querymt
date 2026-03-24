use base64::{
    Engine as _, engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE_NO_PAD,
};
use heck::ToSnakeCase;
use http::{
    Method, Request, Response,
    header::{AUTHORIZATION, CONTENT_TYPE},
};
use log::debug;
use querymt::{
    FunctionCall, ToolCall, Usage,
    chat::{
        ChatMessage, ChatResponse, ChatRole, Content, FinishReason, ReasoningEffort, StreamChunk,
        Tool, ToolChoice,
    },
    error::LLMError,
    handle_http_error,
};
use schemars::{Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use url::Url;

pub fn url_schema(_gen: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "format": "uri"
    })
}

fn should_snakecase_extra_body(base_url: &Url) -> bool {
    // The Codex Responses API (chatgpt.com/backend-api/codex/) expects snake_case
    // parameter names (e.g. `prompt_cache_key`), but heuristics and user configs
    // may use camelCase (e.g. `promptCacheKey`). Normalize here.
    matches!(base_url.host_str(), Some("chatgpt.com"))
}

fn normalize_extra_body_value(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(normalize_extra_body_map(map)),
        Value::Array(arr) => {
            Value::Array(arr.into_iter().map(normalize_extra_body_value).collect())
        }
        other => other,
    }
}

fn normalize_extra_body_map(map: Map<String, Value>) -> Map<String, Value> {
    // Two-pass to prefer keys that are already snake_case.
    let entries: Vec<(String, Value)> = map.into_iter().collect();
    let mut out = Map::with_capacity(entries.len());

    for (k, v) in &entries {
        let nk = k.to_snake_case();
        if &nk == k {
            out.insert(k.clone(), normalize_extra_body_value(v.clone()));
        }
    }

    for (k, v) in entries {
        let nk = k.to_snake_case();
        if nk != k && !out.contains_key(&nk) {
            out.insert(nk, normalize_extra_body_value(v));
        }
    }

    out
}

pub trait CodexProviderConfig {
    fn api_key(&self) -> String;
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
    fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        None
    }
    fn extra_body(&self) -> Option<Map<String, Value>> {
        None
    }
}

#[derive(Debug, Clone, Default)]
pub struct CodexToolUseState {
    /// The Responses API item id (e.g. `fc_...`), used to correlate argument delta/done events.
    pub item_id: Option<String>,
    /// The tool call id (e.g. `call_...`), which we surface as `ToolCall.id`.
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
        role: Cow<'a, str>,
        content: Vec<CodexInputContent<'a>>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: Cow<'a, str>,
        name: Cow<'a, str>,
        arguments: Cow<'a, str>,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput {
        call_id: Cow<'a, str>,
        output: Cow<'a, str>,
    },
}

#[derive(Serialize, Debug)]
#[serde(tag = "type")]
enum CodexInputContent<'a> {
    /// User-turn text: `{ "type": "input_text", "text": "..." }`
    #[serde(rename = "input_text")]
    InputText { text: Cow<'a, str> },
    /// Assistant-turn text: `{ "type": "output_text", "text": "..." }`
    #[serde(rename = "output_text")]
    OutputText { text: Cow<'a, str> },
    /// Inline image: `{ "type": "input_image", "image_url": "data:...;base64,..." }`
    #[serde(rename = "input_image")]
    InputImage { image_url: Cow<'a, str> },
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
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    extra_body: Option<Map<String, Value>>,
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
    usage: Option<CodexRawUsage>,
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
    #[serde(default, alias = "reasoning", alias = "reasoning_content")]
    text: Option<String>,
    tool_calls: Vec<ToolCall>,
}

/// Raw usage response from Codex API, before normalization.
#[derive(Deserialize, Debug, Clone)]
struct CodexRawUsage {
    input_tokens: u32,
    output_tokens: u32,
    #[serde(default)]
    input_tokens_details: Option<CodexInputTokensDetails>,
    #[serde(default)]
    output_tokens_details: Option<CodexOutputTokensDetails>,
}

#[derive(Deserialize, Debug, Clone, Default)]
struct CodexInputTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

#[derive(Deserialize, Debug, Clone, Default)]
struct CodexOutputTokensDetails {
    #[serde(default)]
    reasoning_tokens: u32,
}

impl CodexRawUsage {
    fn into_usage(self) -> Usage {
        let cache_read = self
            .input_tokens_details
            .map(|d| d.cached_tokens)
            .unwrap_or(0);
        let reasoning = self
            .output_tokens_details
            .map(|d| d.reasoning_tokens)
            .unwrap_or(0);

        let usage = Usage {
            input_tokens: self.input_tokens.saturating_sub(cache_read),
            output_tokens: self.output_tokens.saturating_sub(reasoning),
            reasoning_tokens: reasoning,
            cache_read,
            cache_write: 0,
        };
        usage
    }
}

#[derive(Deserialize, Debug)]
struct CodexSseEvent {
    #[serde(rename = "type")]
    kind: String,
    delta: Option<String>,
    arguments: Option<String>,
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
                    if (item.content_type == "output_text" || item.content_type == "text")
                        && let Some(text) = &item.text
                    {
                        pieces.push(text.clone());
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

    fn thinking(&self) -> Option<String> {
        let mut thoughts = Vec::new();
        for output in &self.output {
            if output.output_type != "message" {
                continue;
            }
            if let Some(content) = &output.content {
                for item in content {
                    if (item.content_type == "reasoning"
                        || item.content_type == "reasoning_text"
                        || item.content_type == "thinking")
                        && let Some(text) = &item.text
                    {
                        thoughts.push(text.clone());
                    }
                }
            }
        }
        if thoughts.is_empty() {
            None
        } else {
            Some(thoughts.join(""))
        }
    }

    fn tool_calls(&self) -> Option<Vec<ToolCall>> {
        //self.output.iter().flat_map(|c| c.content).collect();
        None
    }

    fn usage(&self) -> Option<Usage> {
        self.usage.clone().map(|u| u.into_usage())
    }

    fn finish_reason(&self) -> Option<FinishReason> {
        None
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
        inputs.push(CodexInputItem::Message {
            role: Cow::Borrowed("user"),
            content: vec![CodexInputContent::InputText {
                text: Cow::Owned(text),
            }],
        });
    }
    for msg in messages {
        let is_user = matches!(msg.role, ChatRole::User);

        // ── Pass 1: collect regular content blocks into a single message item ──
        // ToolUse and ToolResult are emitted as separate API items in pass 2.
        let mut content_blocks: Vec<CodexInputContent> = Vec::new();

        for block in &msg.content {
            match block {
                Content::Text { text } if !text.is_empty() => {
                    content_blocks.push(if is_user {
                        CodexInputContent::InputText {
                            text: Cow::Borrowed(text.as_str()),
                        }
                    } else {
                        CodexInputContent::OutputText {
                            text: Cow::Borrowed(text.as_str()),
                        }
                    });
                }
                Content::Image { mime_type, data } => {
                    let data_url = format!("data:{};base64,{}", mime_type, STANDARD.encode(data));
                    content_blocks.push(CodexInputContent::InputImage {
                        image_url: Cow::Owned(data_url),
                    });
                }
                Content::ImageUrl { url } => {
                    content_blocks.push(CodexInputContent::InputImage {
                        image_url: Cow::Borrowed(url.as_str()),
                    });
                }
                // ToolUse, ToolResult, Thinking — handled in pass 2 or skipped.
                _ => {}
            }
        }

        if !content_blocks.is_empty() {
            let role = if is_user { "user" } else { "assistant" };
            inputs.push(CodexInputItem::Message {
                role: Cow::Borrowed(role),
                content: content_blocks,
            });
        }

        // ── Pass 2: emit function_call / function_call_output items ──────────
        for block in &msg.content {
            match block {
                Content::ToolUse {
                    id,
                    name,
                    arguments,
                } => {
                    inputs.push(CodexInputItem::FunctionCall {
                        call_id: Cow::Borrowed(id.as_str()),
                        name: Cow::Borrowed(name.as_str()),
                        arguments: Cow::Owned(serde_json::to_string(arguments).unwrap_or_default()),
                    });
                }
                Content::ToolResult { id, content, .. } => {
                    // function_call_output only accepts a plain string; collect text and
                    // describe media blocks, then inject media as a follow-up user message
                    // so the model can actually see it.
                    let mut text_parts: Vec<String> = Vec::new();
                    let mut media_blocks: Vec<CodexInputContent> = Vec::new();

                    for c in content {
                        match c {
                            Content::Text { text } => {
                                text_parts.push(text.clone());
                            }
                            Content::Image { mime_type, data } => {
                                text_parts.push(format!(
                                    "[Image: {} ({} bytes) — content follows in next message]",
                                    mime_type,
                                    data.len()
                                ));
                                media_blocks.push(CodexInputContent::InputImage {
                                    image_url: Cow::Owned(format!(
                                        "data:{};base64,{}",
                                        mime_type,
                                        STANDARD.encode(data)
                                    )),
                                });
                            }
                            _ => {}
                        }
                    }

                    inputs.push(CodexInputItem::FunctionCallOutput {
                        call_id: Cow::Borrowed(id.as_str()),
                        output: Cow::Owned(text_parts.join("\n")),
                    });

                    // Inject media as a follow-up user message so the model can see it.
                    if !media_blocks.is_empty() {
                        let mut follow_up = vec![CodexInputContent::InputText {
                            text: Cow::Owned(format!("[Content from tool call {}]", id)),
                        }];
                        follow_up.extend(media_blocks);
                        inputs.push(CodexInputItem::Message {
                            role: Cow::Borrowed("user"),
                            content: follow_up,
                        });
                    }
                }
                // Audio, ResourceLink, Thinking — not supported; skip.
                _ => {}
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

    let extra_body = {
        let mut merged = cfg.extra_body().unwrap_or_default();

        if let Some(effort) = cfg.reasoning_effort() {
            merged.insert(
                "reasoning".into(),
                serde_json::json!({ "effort": codex_effort_str(effort) }),
            );
        }

        if merged.is_empty() {
            None
        } else if should_snakecase_extra_body(cfg.base_url()) {
            Some(normalize_extra_body_map(merged))
        } else {
            Some(merged)
        }
    };

    let body = CodexChatRequest {
        model: cfg.model(),
        input: inputs,
        instructions,
        store: false,
        max_output_tokens: cfg.max_tokens().copied(),
        temperature: cfg.temperature().copied(),
        // Codex backend requires streaming.
        stream: true,
        top_p: cfg.top_p().copied(),
        top_k: cfg.top_k().copied(),
        tools: request_tools,
        tool_choice: request_tool_choice,
        extra_body,
    };

    let json_body = serde_json::to_vec(&body)?;
    let url = cfg
        .base_url()
        .join("responses")
        .map_err(|e| LLMError::HttpError(e.to_string()))?;
    let api_key = cfg.api_key();
    let account_id = chatgpt_account_id(&api_key)?;

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

pub fn codex_parse_chat_with_state(
    response: Response<Vec<u8>>,
    tool_state_buffer: &Arc<Mutex<HashMap<usize, CodexToolUseState>>>,
) -> Result<Box<dyn ChatResponse>, LLMError> {
    handle_http_error!(response);

    let body = response.body();
    let raw = String::from_utf8_lossy(body);

    // Codex `responses` endpoint is streaming-only; non-streaming calls must go through
    // the `chat_stream` pipeline.
    if raw.contains("data: ") {
        if let Ok(mut buf) = tool_state_buffer.lock() {
            buf.clear();
        }
        return Err(LLMError::NotImplemented(
            "Codex backend is streaming-only; call chat_stream(_with_tools)".to_string(),
        ));
    }

    let json_resp: Result<CodexChatResponse, serde_json::Error> = serde_json::from_slice(body);
    match json_resp {
        Ok(response) => Ok(Box::new(response)),
        Err(e) => Err(LLMError::ResponseFormatError {
            message: format!("Failed to decode Codex API response: {}", e),
            raw_response: raw.into_owned(),
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

        if event.kind.contains("reasoning") || event.kind.contains("thinking") {
            debug!(
                "codex stream: received reasoning event kind={} has_delta={} output_index={:?} item_id={:?}",
                event.kind,
                event.delta.as_ref().map(|d| !d.is_empty()).unwrap_or(false),
                event.output_index,
                event.item_id
            );
        }

        match event.kind.as_str() {
            "response.output_text.delta" => {
                if let Some(delta) = event.delta {
                    debug!(
                        "codex stream: emitting text delta len={} output_index={:?}",
                        delta.len(),
                        event.output_index
                    );
                    results.push(StreamChunk::Text(delta));
                }
            }
            // Codex reasoning-capable models stream thought deltas with reasoning event types.
            "response.reasoning.delta"
            | "response.reasoning_text.delta"
            | "response.output_reasoning.delta" => {
                if let Some(delta) = event.delta {
                    debug!(
                        "codex stream: emitting thinking delta kind={} len={} output_index={:?} item_id={:?}",
                        event.kind,
                        delta.len(),
                        event.output_index,
                        event.item_id
                    );
                    results.push(StreamChunk::Thinking(delta));
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
                    event.arguments.as_deref(),
                    &mut results,
                    tool_state_buffer,
                );
            }
            "response.completed" => {
                debug!("codex stream: response.completed received");
                if let Some(response) = event.response {
                    emit_tool_calls_from_response(&response, &mut results, tool_state_buffer);
                    if let Some(usage_value) = response.get("usage")
                        && let Ok(usage) = serde_json::from_value::<Usage>(usage_value.clone())
                    {
                        results.push(StreamChunk::Usage(usage));
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

    let item_id = item.get("id").and_then(Value::as_str).map(str::to_string);
    let call_id = item
        .get("call_id")
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
    if let Some(item_id) = item_id {
        state.item_id = Some(item_id);
    }
    if let Some(call_id) = call_id {
        state.id = Some(call_id);
    }
    if let Some(name) = name {
        state.name = Some(name);
    }
    if !state.started
        && let (Some(id), Some(name)) = (state.id.clone(), state.name.clone())
    {
        state.started = true;
        results.push(StreamChunk::ToolUseStart { index, id, name });
    }

    // `response.output_item.added` typically includes empty arguments; only complete when
    // the backend provides non-empty JSON arguments (e.g. `response.output_item.done`).
    if let Some(arguments) = arguments
        && !arguments.trim().is_empty()
    {
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
    if !state.started
        && let (Some(id), Some(name)) = (state.id.clone(), state.name.clone())
    {
        state.started = true;
        results.push(StreamChunk::ToolUseStart { index, id, name });
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
    arguments: Option<&str>,
    results: &mut Vec<StreamChunk>,
    tool_state_buffer: &Arc<Mutex<HashMap<usize, CodexToolUseState>>>,
) {
    let index = resolve_tool_index_with_item(output_index, item_id, tool_state_buffer);
    let mut buffer = tool_state_buffer.lock().unwrap();
    if let Some(state) = buffer.get_mut(&index) {
        if !state.started
            && let (Some(id), Some(name)) = (state.id.clone(), state.name.clone())
        {
            state.started = true;
            results.push(StreamChunk::ToolUseStart { index, id, name });
        }
        if let Some(arguments) = arguments
            && !arguments.trim().is_empty()
        {
            emit_arguments_delta(index, arguments, state, results);
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

        let item_id = item.get("id").and_then(Value::as_str).map(str::to_string);
        let id = item
            .get("call_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let name = item.get("name").and_then(Value::as_str).map(str::to_string);
        let arguments = item.get("arguments").and_then(Value::as_str).unwrap_or("");

        let index = resolve_tool_index(Some(idx), tool_state_buffer);
        let mut buffer = tool_state_buffer.lock().unwrap();
        let state = buffer.entry(index).or_default();
        if let Some(item_id) = item_id {
            state.item_id = Some(item_id);
        }
        if let Some(id) = id {
            state.id = Some(id);
        }
        if let Some(name) = name {
            state.name = Some(name);
        }
        if !state.started
            && let (Some(id), Some(name)) = (state.id.clone(), state.name.clone())
        {
            state.started = true;
            results.push(StreamChunk::ToolUseStart { index, id, name });
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
            .find(|(_, state)| state.item_id.as_deref() == Some(item_id))
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
    let account_id = chatgpt_account_id(&api_key)?;

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
        serde_json::from_slice(response.body());
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

/// Map unified `ReasoningEffort` to the Codex API string.
///
/// Codex uses `"xhigh"` where the unified enum uses `Max`.
/// `None` is handled by the caller (omit field = provider/model default).
fn codex_effort_str(e: ReasoningEffort) -> &'static str {
    match e {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Max => "xhigh",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CodexChatResponse, CodexProviderConfig, CodexToolUseState, codex_chat_request,
        codex_parse_stream_chunk_with_state,
    };
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use querymt::chat::{
        ChatMessage, ChatResponse, ChatRole, Content, ReasoningEffort, StreamChunk, Tool,
        ToolChoice,
    };
    use serde_json::Value;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use url::Url;

    // ── Minimal config stub for unit tests ───────────────────────────────────

    struct TestConfig {
        api_key: String,
        base_url: Url,
    }

    impl TestConfig {
        fn new() -> Self {
            // Build a minimal fake JWT: header.payload.signature
            // payload must contain `https://api.openai.com/auth.chatgpt_account_id`
            let payload_json =
                r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"test-account-id"}}"#;
            let payload_b64 = URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
            let api_key = format!("eyJ.{}.sig", payload_b64);
            Self {
                api_key,
                base_url: Url::parse("https://chatgpt.com/backend-api/codex/").unwrap(),
            }
        }
    }

    impl CodexProviderConfig for TestConfig {
        fn api_key(&self) -> String {
            self.api_key.clone()
        }
        fn base_url(&self) -> &Url {
            &self.base_url
        }
        fn model(&self) -> &str {
            "codex-mini-latest"
        }
        fn max_tokens(&self) -> Option<&u32> {
            None
        }
        fn temperature(&self) -> Option<&f32> {
            None
        }
        fn instructions(&self) -> Option<&str> {
            None
        }
        fn system(&self) -> Option<&str> {
            None
        }
        fn timeout_seconds(&self) -> Option<&u64> {
            None
        }
        fn stream(&self) -> Option<&bool> {
            None
        }
        fn top_p(&self) -> Option<&f32> {
            None
        }
        fn top_k(&self) -> Option<&u32> {
            None
        }
        fn tools(&self) -> Option<&[Tool]> {
            None
        }
        fn tool_choice(&self) -> Option<&ToolChoice> {
            None
        }
        fn client_version(&self) -> Option<&str> {
            None
        }
        fn reasoning_effort(&self) -> Option<ReasoningEffort> {
            None
        }
        fn extra_body(&self) -> Option<serde_json::Map<String, Value>> {
            None
        }
    }

    /// Build a user `ChatMessage` whose content is a single `Content::ToolResult`
    /// wrapping the given inner blocks.
    fn tool_result_msg(call_id: &str, inner: Vec<Content>) -> ChatMessage {
        ChatMessage {
            role: ChatRole::User,
            content: vec![Content::ToolResult {
                id: call_id.to_string(),
                name: Some("read_tool".to_string()),
                is_error: false,
                content: inner,
            }],
            cache: None,
        }
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Find the first input item of the given type in the serialised request body.
    fn find_input_item<'a>(body: &'a Value, item_type: &str) -> Option<&'a Value> {
        body["input"]
            .as_array()?
            .iter()
            .find(|item| item["type"] == item_type)
    }

    /// Collect ALL input items of the given type.
    fn all_input_items<'a>(body: &'a Value, item_type: &str) -> Vec<&'a Value> {
        body["input"]
            .as_array()
            .map(|arr| arr.iter().filter(|i| i["type"] == item_type).collect())
            .unwrap_or_default()
    }

    // ── tool-result image tests ───────────────────────────────────────────────

    /// When a tool result contains an image, the function_call_output must have a
    /// descriptive text referencing the follow-up, AND a follow-up user message
    /// must carry the image as an `input_image` content block.
    #[test]
    fn codex_tool_result_with_image_injects_follow_up_message() {
        let cfg = TestConfig::new();
        let png_bytes: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x00];

        let messages = vec![tool_result_msg(
            "call-1",
            vec![Content::Image {
                mime_type: "image/png".to_string(),
                data: png_bytes.clone(),
            }],
        )];

        let body: Value = serde_json::from_slice(
            codex_chat_request(&cfg, &messages, None)
                .expect("must not error on image tool result")
                .body(),
        )
        .unwrap();

        // function_call_output must be present and non-empty
        let fco_output = find_input_item(&body, "function_call_output")
            .and_then(|i| i["output"].as_str())
            .expect("function_call_output with output field");
        assert!(!fco_output.is_empty(), "output must not be empty");

        // A follow-up message item must exist after function_call_output
        let msgs = all_input_items(&body, "message");
        let follow_up = msgs
            .iter()
            .find(|m| {
                m["content"]
                    .as_array()
                    .map(|c| c.iter().any(|b| b["type"] == "input_image"))
                    .unwrap_or(false)
            })
            .expect("a follow-up user message with an input_image block");

        // The input_image block must carry a data URL
        let image_url = follow_up["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["type"] == "input_image")
            .and_then(|b| b["image_url"].as_str())
            .expect("input_image block with image_url");

        assert!(
            image_url.starts_with("data:image/png;base64,"),
            "image_url must be a base64 data URL, got: {}",
            &image_url[..50.min(image_url.len())]
        );
    }

    /// When a tool result contains both text and an image, text appears in
    /// function_call_output and the image appears in the follow-up message.
    #[test]
    fn codex_tool_result_with_text_and_image_preserves_text_and_injects_image() {
        let cfg = TestConfig::new();

        let messages = vec![tool_result_msg(
            "call-3",
            vec![
                Content::text("some text output"),
                Content::Image {
                    mime_type: "image/jpeg".to_string(),
                    data: vec![0xFF, 0xD8, 0xFF],
                },
            ],
        )];

        let body: Value = serde_json::from_slice(
            codex_chat_request(&cfg, &messages, None)
                .expect("must not error on mixed text+image tool result")
                .body(),
        )
        .unwrap();

        let fco_output = find_input_item(&body, "function_call_output")
            .and_then(|i| i["output"].as_str())
            .unwrap();

        assert!(
            fco_output.contains("some text output"),
            "original text must be in function_call_output, got: {}",
            fco_output
        );

        // Follow-up message with input_image must exist
        let msgs = all_input_items(&body, "message");
        assert!(
            msgs.iter().any(|m| m["content"]
                .as_array()
                .map(|c| c.iter().any(|b| b["type"] == "input_image"))
                .unwrap_or(false)),
            "follow-up message with input_image must be injected"
        );
    }

    // ── top-level image tests ────────────────────────────────────────────────

    /// A top-level Content::Image in a user message must be serialized as an
    /// `input_image` content block — not skipped, not errored.
    #[test]
    fn codex_top_level_image_serialized_as_input_image() {
        let cfg = TestConfig::new();

        let messages = vec![ChatMessage {
            role: ChatRole::User,
            content: vec![
                Content::text("describe this"),
                Content::Image {
                    mime_type: "image/png".to_string(),
                    data: vec![0x89, 0x50, 0x4E, 0x47],
                },
            ],
            cache: None,
        }];

        let body: Value = serde_json::from_slice(
            codex_chat_request(&cfg, &messages, None)
                .expect("must not error on top-level image")
                .body(),
        )
        .unwrap();

        let msg = find_input_item(&body, "message").expect("message item");
        let content = msg["content"].as_array().expect("content array");

        let has_text = content.iter().any(|b| b["type"] == "input_text");
        let image_block = content.iter().find(|b| b["type"] == "input_image");

        assert!(has_text, "input_text block must be present");
        assert!(image_block.is_some(), "input_image block must be present");
        assert!(
            image_block.unwrap()["image_url"]
                .as_str()
                .unwrap_or("")
                .starts_with("data:image/png;base64,"),
            "image_url must be a base64 data URL"
        );
    }

    /// A top-level Content::ImageUrl must be serialized as an `input_image` block
    /// with the URL passed through directly (no base64 encoding).
    #[test]
    fn codex_top_level_image_url_serialized_as_input_image() {
        let cfg = TestConfig::new();

        let messages = vec![ChatMessage {
            role: ChatRole::User,
            content: vec![Content::ImageUrl {
                url: "https://example.com/img.png".to_string(),
            }],
            cache: None,
        }];

        let body: Value = serde_json::from_slice(
            codex_chat_request(&cfg, &messages, None)
                .expect("must not error on top-level ImageUrl")
                .body(),
        )
        .unwrap();

        let msg = find_input_item(&body, "message").expect("message item");
        let image_block = msg["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["type"] == "input_image")
            .expect("input_image block");

        assert_eq!(
            image_block["image_url"].as_str(),
            Some("https://example.com/img.png")
        );
    }

    #[test]
    fn codex_chat_response_exposes_reasoning_as_thinking() {
        let body = br#"{
            "output": [{
                "type": "message",
                "content": [
                    {"type": "reasoning", "text": "think 1", "tool_calls": []},
                    {"type": "reasoning_text", "text": " + think 2", "tool_calls": []},
                    {"type": "output_text", "text": "answer", "tool_calls": []}
                ]
            }]
        }"#;

        let response: CodexChatResponse = serde_json::from_slice(body).unwrap();
        assert_eq!(response.text().as_deref(), Some("answer"));
        assert_eq!(response.thinking().as_deref(), Some("think 1 + think 2"));
    }

    #[test]
    fn codex_streaming_emits_thinking_deltas() {
        let state = Arc::new(Mutex::new(HashMap::<usize, CodexToolUseState>::new()));
        let chunk = br#"data: {"type":"response.reasoning.delta","delta":"thought "}

data: {"type":"response.output_text.delta","delta":"answer"}

data: {"type":"response.reasoning_text.delta","delta":"continued"}

"#;

        let events = codex_parse_stream_chunk_with_state(chunk, &state).unwrap();
        assert_eq!(events.len(), 3);

        match &events[0] {
            StreamChunk::Thinking(text) => assert_eq!(text, "thought "),
            other => panic!("expected thinking chunk, got {other:?}"),
        }
        match &events[1] {
            StreamChunk::Text(text) => assert_eq!(text, "answer"),
            other => panic!("expected text chunk, got {other:?}"),
        }
        match &events[2] {
            StreamChunk::Thinking(text) => assert_eq!(text, "continued"),
            other => panic!("expected thinking chunk, got {other:?}"),
        }
    }

    #[test]
    fn codex_effort_str_maps_correctly() {
        use super::{ReasoningEffort, codex_effort_str};
        assert_eq!(codex_effort_str(ReasoningEffort::Low), "low");
        assert_eq!(codex_effort_str(ReasoningEffort::Medium), "medium");
        assert_eq!(codex_effort_str(ReasoningEffort::High), "high");
        // Max must map to "xhigh" — Codex API does not accept "max"
        assert_eq!(codex_effort_str(ReasoningEffort::Max), "xhigh");
    }
}
