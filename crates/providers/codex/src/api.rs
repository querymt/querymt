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
    pub emitted_thinking: Vec<String>,
    pub emitted_thinking_text: String,
    pub streamed_thinking_seen: bool,
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
        output: CodexFunctionCallOutput<'a>,
    },
}

#[derive(Serialize, Debug)]
#[serde(untagged)]
enum CodexFunctionCallOutput<'a> {
    Text(Cow<'a, str>),
    Parts(Vec<CodexToolOutputPart<'a>>),
}

#[derive(Serialize, Debug)]
#[serde(tag = "type")]
enum CodexToolOutputPart<'a> {
    #[serde(rename = "output_text")]
    OutputText { text: Cow<'a, str> },
    #[serde(rename = "input_image")]
    InputImage {
        image_url: Cow<'a, str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<&'a str>,
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
    // TODO: Add `detail` support once image detail is modeled in shared content/config.
    // TODO: Support file IDs here so Codex can accept images, PDFs, and other uploaded files.
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
    #[serde(default, alias = "reasoning", alias = "reasoning_content")]
    text: Option<String>,
    #[serde(default)]
    summary: Option<Vec<CodexReasoningSummary>>,
}

#[derive(Deserialize, Debug)]
struct CodexReasoningSummary {
    text: Option<String>,
}

#[derive(Deserialize, Debug)]
struct CodexOutputContent {
    #[serde(rename = "type")]
    content_type: String,
    #[serde(default, alias = "reasoning", alias = "reasoning_content")]
    text: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
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

        Usage {
            input_tokens: self.input_tokens.saturating_sub(cache_read),
            output_tokens: self.output_tokens.saturating_sub(reasoning),
            reasoning_tokens: reasoning,
            cache_read,
            cache_write: 0,
        }
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

fn joined_non_empty(pieces: Vec<String>) -> Option<String> {
    if pieces.is_empty() {
        None
    } else {
        Some(pieces.join(""))
    }
}

fn collect_codex_output_reasoning(output: &CodexOutput, thoughts: &mut Vec<String>) {
    if output.output_type == "reasoning" {
        if let Some(text) = &output.text
            && !text.is_empty()
        {
            thoughts.push(text.clone());
        }
        if let Some(summary) = &output.summary {
            for item in summary {
                if let Some(text) = &item.text
                    && !text.is_empty()
                {
                    thoughts.push(text.clone());
                }
            }
        }
        if let Some(content) = &output.content {
            for item in content {
                if (item.content_type == "reasoning_text" || item.content_type == "text")
                    && let Some(text) = &item.text
                    && !text.is_empty()
                {
                    thoughts.push(text.clone());
                }
            }
        }
    }

    if output.output_type != "message" {
        return;
    }

    if let Some(content) = &output.content {
        for item in content {
            if (item.content_type == "reasoning"
                || item.content_type == "reasoning_text"
                || item.content_type == "thinking")
                && let Some(text) = &item.text
                && !text.is_empty()
            {
                thoughts.push(text.clone());
            }
        }
    }
}

fn extract_reasoning_text_from_value(value: &Value) -> Option<String> {
    let mut thoughts = Vec::new();
    collect_reasoning_text_from_value(value, &mut thoughts);
    joined_non_empty(thoughts)
}

fn merge_codex_reasoning_request(merged: &mut Map<String, Value>, effort: Option<ReasoningEffort>) {
    let mut reasoning = match merged.remove("reasoning") {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };

    reasoning
        .entry("summary".to_string())
        .or_insert_with(|| Value::String("auto".to_string()));

    if let Some(effort) = effort {
        reasoning.insert(
            "effort".to_string(),
            Value::String(codex_effort_str(effort).to_string()),
        );
    }

    merged.insert("reasoning".to_string(), Value::Object(reasoning));
}

fn merge_codex_reasoning_include(merged: &mut Map<String, Value>) {
    const REASONING_INCLUDE: &str = "reasoning.encrypted_content";

    match merged.get_mut("include") {
        Some(Value::Array(include)) => {
            if !include
                .iter()
                .any(|item| item.as_str() == Some(REASONING_INCLUDE))
            {
                include.push(Value::String(REASONING_INCLUDE.to_string()));
            }
        }
        _ => {
            merged.insert(
                "include".to_string(),
                Value::Array(vec![Value::String(REASONING_INCLUDE.to_string())]),
            );
        }
    }
}

fn collect_reasoning_text_from_value(value: &Value, thoughts: &mut Vec<String>) {
    if let Some(text) = value.get("text").and_then(Value::as_str)
        && !text.is_empty()
    {
        thoughts.push(text.to_string());
    }
    for key in ["reasoning", "reasoning_content"] {
        if let Some(text) = value.get(key).and_then(Value::as_str)
            && !text.is_empty()
        {
            thoughts.push(text.to_string());
        }
    }
    if let Some(summary) = value.get("summary").and_then(Value::as_array) {
        for item in summary {
            if let Some(text) = item.get("text").and_then(Value::as_str)
                && !text.is_empty()
            {
                thoughts.push(text.to_string());
            }
        }
    }
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        for item in content {
            if matches!(
                item.get("type").and_then(Value::as_str),
                Some("reasoning_text" | "text")
            ) && let Some(text) = item.get("text").and_then(Value::as_str)
                && !text.is_empty()
            {
                thoughts.push(text.to_string());
            }
        }
    }
}

fn extract_reasoning_text_from_response(response: &Value) -> Option<String> {
    let mut thoughts = Vec::new();
    if let Some(items) = response.get("output").and_then(Value::as_array) {
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                collect_reasoning_text_from_value(item, &mut thoughts);
            }
        }
    }
    joined_non_empty(thoughts)
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
            collect_codex_output_reasoning(output, &mut thoughts);
        }
        joined_non_empty(thoughts)
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

fn codex_chat_body_json<C: CodexProviderConfig>(
    cfg: &C,
    messages: &[ChatMessage],
    tools: Option<&[Tool]>,
) -> Result<Vec<u8>, LLMError> {
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
                    let mut output_parts: Vec<CodexToolOutputPart> = Vec::new();
                    let mut text_only_parts: Vec<String> = Vec::new();
                    let mut has_non_text = false;

                    for c in content {
                        match c {
                            Content::Text { text } => {
                                output_parts.push(CodexToolOutputPart::OutputText {
                                    text: Cow::Borrowed(text.as_str()),
                                });
                                text_only_parts.push(text.clone());
                            }
                            Content::Image { mime_type, data } => {
                                has_non_text = true;
                                output_parts.push(CodexToolOutputPart::InputImage {
                                    image_url: Cow::Owned(format!(
                                        "data:{};base64,{}",
                                        mime_type,
                                        STANDARD.encode(data)
                                    )),
                                    detail: None,
                                });
                            }
                            Content::ImageUrl { url } => {
                                has_non_text = true;
                                output_parts.push(CodexToolOutputPart::InputImage {
                                    image_url: Cow::Borrowed(url.as_str()),
                                    detail: None,
                                });
                            }
                            Content::Pdf { data } => {
                                has_non_text = true;
                                output_parts.push(CodexToolOutputPart::OutputText {
                                    text: Cow::Owned(format!(
                                        "[PDF tool output not yet serialized natively ({} bytes)]",
                                        data.len()
                                    )),
                                });
                            }
                            Content::Audio { mime_type, data } => {
                                has_non_text = true;
                                output_parts.push(CodexToolOutputPart::OutputText {
                                    text: Cow::Owned(format!(
                                        "[Audio tool output not yet serialized natively ({}: {} bytes)]",
                                        mime_type,
                                        data.len()
                                    )),
                                });
                            }
                            _ => {}
                        }
                    }

                    let output = if has_non_text {
                        CodexFunctionCallOutput::Parts(output_parts)
                    } else {
                        CodexFunctionCallOutput::Text(Cow::Owned(text_only_parts.join("\n")))
                    };

                    inputs.push(CodexInputItem::FunctionCallOutput {
                        call_id: Cow::Borrowed(id.as_str()),
                        output,
                    });
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

        merge_codex_reasoning_request(&mut merged, cfg.reasoning_effort());
        merge_codex_reasoning_include(&mut merged);

        if should_snakecase_extra_body(cfg.base_url()) {
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

    Ok(serde_json::to_vec(&body)?)
}

pub fn codex_chat_request<C: CodexProviderConfig>(
    cfg: &C,
    messages: &[ChatMessage],
    tools: Option<&[Tool]>,
) -> Result<Request<Vec<u8>>, LLMError> {
    let json_body = codex_chat_body_json(cfg, messages, tools)?;
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
            let has_tool_calls = tool_state_buffer
                .lock()
                .unwrap()
                .values()
                .any(|s| s.started);
            clear_thinking_state(tool_state_buffer);
            results.push(StreamChunk::Done {
                finish_reason: if has_tool_calls {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                },
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

        if event.kind.contains("reasoning") && event.kind.contains("delta") {
            if let Some(delta) = event.delta.filter(|delta| !delta.is_empty()) {
                debug!(
                    "codex stream: emitting thinking delta kind={} len={} output_index={:?} item_id={:?}",
                    event.kind,
                    delta.len(),
                    event.output_index,
                    event.item_id
                );
                mark_streamed_thinking_emitted(tool_state_buffer, &delta);
                results.push(StreamChunk::Thinking(delta));
            }
            continue;
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
            "response.output_item.done" => {
                if let Some(item) = event.item {
                    if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                        if let Some(text) = extract_reasoning_text_from_value(&item)
                            && let Some(text) =
                                mark_final_thinking_emitted(tool_state_buffer, &text)
                        {
                            results.push(StreamChunk::Thinking(text));
                        }
                    } else {
                        handle_output_item_event(
                            &item,
                            event.output_index,
                            &mut results,
                            tool_state_buffer,
                        );
                    }
                }
            }
            "response.output_item.added" => {
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
                    if let Some(text) = extract_reasoning_text_from_response(&response)
                        && let Some(text) = mark_final_thinking_emitted(tool_state_buffer, &text)
                    {
                        results.push(StreamChunk::Thinking(text));
                    }
                    if let Some(usage_value) = response.get("usage")
                        && let Ok(usage) =
                            serde_json::from_value::<CodexRawUsage>(usage_value.clone())
                    {
                        results.push(StreamChunk::Usage(usage.into_usage()));
                    }
                }
                let finish_reason = if tool_state_buffer
                    .lock()
                    .unwrap()
                    .values()
                    .any(|s| s.started)
                {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                };
                clear_thinking_state(tool_state_buffer);
                results.push(StreamChunk::Done { finish_reason });
            }
            "response.failed" => {
                let message = event
                    .response
                    .as_ref()
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Codex response failed");
                clear_thinking_state(tool_state_buffer);
                return Err(LLMError::ProviderError(message.to_string()));
            }
            _ => {}
        }
    }

    Ok(results)
}

fn mark_streamed_thinking_emitted(
    tool_state_buffer: &Arc<Mutex<HashMap<usize, CodexToolUseState>>>,
    text: &str,
) {
    let mut buffer = tool_state_buffer.lock().unwrap();
    let state = buffer.entry(usize::MAX).or_default();
    state.streamed_thinking_seen = true;
    state.emitted_thinking.push(text.to_string());
    state.emitted_thinking_text.push_str(text);
}

fn mark_final_thinking_emitted(
    tool_state_buffer: &Arc<Mutex<HashMap<usize, CodexToolUseState>>>,
    text: &str,
) -> Option<String> {
    let mut buffer = tool_state_buffer.lock().unwrap();
    let state = buffer.entry(usize::MAX).or_default();
    let text_to_emit = if state.streamed_thinking_seen {
        let emitted = state.emitted_thinking_text.as_str();
        if emitted == text || emitted.contains(text) {
            return None;
        }
        text.strip_prefix(emitted).unwrap_or(text).to_string()
    } else {
        if state.emitted_thinking.iter().any(|emitted| emitted == text) {
            return None;
        }
        text.to_string()
    };
    state.emitted_thinking.push(text_to_emit.clone());
    state.emitted_thinking_text.push_str(&text_to_emit);
    Some(text_to_emit)
}

fn clear_thinking_state(tool_state_buffer: &Arc<Mutex<HashMap<usize, CodexToolUseState>>>) {
    tool_state_buffer.lock().unwrap().remove(&usize::MAX);
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
        CodexChatResponse, CodexToolUseState, chatgpt_account_id, codex_chat_body_json,
        codex_chat_request, codex_parse_stream_chunk_with_state,
    };
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use http::header::AUTHORIZATION;
    use querymt::chat::{ChatMessage, ChatResponse, ChatRole, Content, FinishReason, StreamChunk};
    use serde_json::Value;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use url::Url;

    /// Build a minimal config for unit tests.
    fn test_codex(api_key: &str) -> crate::Codex {
        crate::Codex {
            api_key: api_key.to_string(),
            base_url: Url::parse("https://chatgpt.com/backend-api/codex/").unwrap(),
            model: "codex-mini-latest".to_string(),
            max_tokens: None,
            temperature: None,
            instructions: None,
            system: None,
            timeout_seconds: None,
            stream: None,
            top_p: None,
            top_k: None,
            client_version: None,
            tools: None,
            tool_choice: None,
            reasoning_effort: None,
            extra_body: None,
            tool_state_buffer: crate::Codex::default_tool_state_buffer(),
            key_resolver: None,
        }
    }

    fn test_oauth_token(account_id: &str) -> String {
        let payload_json = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account_id,
            }
        });
        let payload_b64 = URL_SAFE_NO_PAD.encode(payload_json.to_string().as_bytes());
        format!("eyJ.{}.sig", payload_b64)
    }

    #[test]
    fn codex_chat_request_adds_auth_headers() {
        let token = test_oauth_token("test-account-id");
        let codex = test_codex(&token);
        let messages = vec![ChatMessage::user().text("hello").build()];

        let req = codex_chat_request(&codex, &messages, None).expect("chat request should build");

        let expected_auth = format!("Bearer {}", token);
        assert_eq!(
            req.headers()
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some(expected_auth.as_str())
        );
        assert_eq!(
            req.headers()
                .get("ChatGPT-Account-ID")
                .and_then(|v| v.to_str().ok()),
            Some("test-account-id")
        );
    }

    #[test]
    fn chatgpt_account_id_requires_oauth_payload_claim() {
        let err = chatgpt_account_id("not-a-jwt").expect_err("invalid token should fail");
        assert!(
            err.to_string().contains("Invalid OAuth access token"),
            "unexpected error: {err}"
        );
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

    fn basic_user_messages() -> Vec<ChatMessage> {
        vec![ChatMessage::user().text("hello").build()]
    }

    fn codex_body(cfg: &crate::Codex) -> Value {
        serde_json::from_slice(
            &codex_chat_body_json(cfg, &basic_user_messages(), None)
                .expect("chat body should serialize"),
        )
        .expect("chat body should be valid json")
    }

    #[test]
    fn codex_chat_request_includes_reasoning_summary_and_encrypted_content_by_default() {
        let cfg = test_codex("test-token");

        let body = codex_body(&cfg);

        assert_eq!(body["reasoning"]["summary"].as_str(), Some("auto"));
        assert_eq!(body["reasoning"].get("effort"), None);
        assert!(
            body["include"]
                .as_array()
                .expect("include array")
                .iter()
                .any(|item| item.as_str() == Some("reasoning.encrypted_content")),
            "include must request encrypted reasoning content"
        );
    }

    #[test]
    fn codex_chat_request_includes_reasoning_effort_and_summary() {
        use super::ReasoningEffort;

        let mut cfg = test_codex("test-token");
        cfg.reasoning_effort = Some(ReasoningEffort::High);

        let body = codex_body(&cfg);

        assert_eq!(body["reasoning"]["effort"].as_str(), Some("high"));
        assert_eq!(body["reasoning"]["summary"].as_str(), Some("auto"));
        assert!(
            body["include"]
                .as_array()
                .expect("include array")
                .iter()
                .any(|item| item.as_str() == Some("reasoning.encrypted_content"))
        );
    }

    #[test]
    fn codex_chat_request_merges_existing_reasoning_and_include_extra_body() {
        use super::ReasoningEffort;

        let mut cfg = test_codex("test-token");
        cfg.reasoning_effort = Some(ReasoningEffort::High);
        cfg.extra_body = Some(
            serde_json::json!({
                "reasoning": {
                    "summary": "detailed",
                    "custom": true
                },
                "include": ["file_search_call.results"],
                "metadata": {"source": "test"}
            })
            .as_object()
            .expect("extra body object")
            .clone(),
        );

        let body = codex_body(&cfg);

        assert_eq!(body["reasoning"]["summary"].as_str(), Some("detailed"));
        assert_eq!(body["reasoning"]["effort"].as_str(), Some("high"));
        assert_eq!(body["reasoning"]["custom"].as_bool(), Some(true));
        let include = body["include"].as_array().expect("include array");
        assert!(
            include
                .iter()
                .any(|item| item.as_str() == Some("file_search_call.results"))
        );
        assert!(
            include
                .iter()
                .any(|item| item.as_str() == Some("reasoning.encrypted_content"))
        );
        assert_eq!(body["metadata"]["source"].as_str(), Some("test"));
    }

    // ── tool-result image tests ───────────────────────────────────────────────

    /// When a tool result contains an image, the function_call_output should carry
    /// the image directly as rich output parts rather than injecting a synthetic
    /// follow-up message.
    #[test]
    fn codex_tool_result_with_image_uses_rich_function_call_output() {
        let cfg = test_codex("test-token");
        let png_bytes: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x00];

        let messages = vec![tool_result_msg(
            "call-1",
            vec![Content::Image {
                mime_type: "image/png".to_string(),
                data: png_bytes.clone(),
            }],
        )];

        let body: Value = serde_json::from_slice(
            &codex_chat_body_json(&cfg, &messages, None)
                .expect("must not error on image tool result"),
        )
        .unwrap();

        // function_call_output must carry rich output parts with an inline image.
        let fco = find_input_item(&body, "function_call_output")
            .expect("function_call_output item must be present");
        let output_parts = fco["output"].as_array().expect("rich output parts array");
        let image_url = output_parts
            .iter()
            .find(|b| b["type"] == "input_image")
            .and_then(|b| b["image_url"].as_str())
            .expect("input_image part with image_url");

        assert!(
            image_url.starts_with("data:image/png;base64,"),
            "image_url must be a base64 data URL, got: {}",
            &image_url[..50.min(image_url.len())]
        );
    }

    /// When a tool result contains both text and an image, the rich
    /// function_call_output must preserve both parts in order.
    #[test]
    fn codex_tool_result_with_text_and_image_preserves_rich_output_order() {
        let cfg = test_codex("test-token");

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
            &codex_chat_body_json(&cfg, &messages, None)
                .expect("must not error on mixed text+image tool result"),
        )
        .unwrap();

        let output_parts = find_input_item(&body, "function_call_output")
            .and_then(|i| i["output"].as_array())
            .expect("rich function_call_output parts");

        assert_eq!(output_parts[0]["type"].as_str(), Some("output_text"));
        assert_eq!(output_parts[0]["text"].as_str(), Some("some text output"));
        assert_eq!(output_parts[1]["type"].as_str(), Some("input_image"));
    }

    // ── top-level image tests ────────────────────────────────────────────────

    /// A top-level Content::Image in a user message must be serialized as an
    /// `input_image` content block — not skipped, not errored.
    #[test]
    fn codex_top_level_image_serialized_as_input_image() {
        let cfg = test_codex("test-token");

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
            &codex_chat_body_json(&cfg, &messages, None)
                .expect("must not error on top-level image"),
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
        let cfg = test_codex("test-token");

        let messages = vec![ChatMessage {
            role: ChatRole::User,
            content: vec![Content::ImageUrl {
                url: "https://example.com/img.png".to_string(),
            }],
            cache: None,
        }];

        let body: Value = serde_json::from_slice(
            &codex_chat_body_json(&cfg, &messages, None)
                .expect("must not error on top-level ImageUrl"),
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
    fn codex_chat_response_exposes_top_level_reasoning_summary_as_thinking() {
        let body = br#"{
            "output": [
                {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "why"}]
                },
                {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "answer", "tool_calls": []}]
                }
            ]
        }"#;

        let response: CodexChatResponse = serde_json::from_slice(body).unwrap();
        assert_eq!(response.text().as_deref(), Some("answer"));
        assert_eq!(response.thinking().as_deref(), Some("why"));
    }

    #[test]
    fn codex_chat_response_exposes_top_level_reasoning_content_as_thinking() {
        let body = br#"{
            "output": [
                {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "why"}],
                    "content": [
                        {"type": "reasoning_text", "text": " because"},
                        {"type": "text", "text": " details"}
                    ],
                    "encrypted_content": "abc"
                },
                {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "answer", "tool_calls": []}]
                }
            ]
        }"#;

        let response: CodexChatResponse = serde_json::from_slice(body).unwrap();
        assert_eq!(response.text().as_deref(), Some("answer"));
        assert_eq!(response.thinking().as_deref(), Some("why because details"));
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
    fn codex_streaming_emits_unknown_reasoning_delta_kinds() {
        let state = Arc::new(Mutex::new(HashMap::<usize, CodexToolUseState>::new()));
        let chunk = br#"data: {"type":"response.reasoning_summary_text.delta","delta":"think"}

"#;

        let events = codex_parse_stream_chunk_with_state(chunk, &state).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamChunk::Thinking(text) => assert_eq!(text, "think"),
            other => panic!("expected thinking chunk, got {other:?}"),
        }
    }

    #[test]
    fn codex_streaming_output_item_done_emits_reasoning_summary() {
        let state = Arc::new(Mutex::new(HashMap::<usize, CodexToolUseState>::new()));
        let chunk = br#"data: {"type":"response.output_item.done","item":{"type":"reasoning","summary":[{"text":"why"}]}}

"#;

        let events = codex_parse_stream_chunk_with_state(chunk, &state).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamChunk::Thinking(text) => assert_eq!(text, "why"),
            other => panic!("expected thinking chunk, got {other:?}"),
        }
    }

    #[test]
    fn codex_streaming_output_item_done_emits_reasoning_content() {
        let state = Arc::new(Mutex::new(HashMap::<usize, CodexToolUseState>::new()));
        let chunk = br#"data: {"type":"response.output_item.done","item":{"type":"reasoning","summary":[{"type":"summary_text","text":"why"}],"content":[{"type":"reasoning_text","text":" because"},{"type":"text","text":" details"}],"encrypted_content":"abc"}}

"#;

        let events = codex_parse_stream_chunk_with_state(chunk, &state).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamChunk::Thinking(text) => assert_eq!(text, "why because details"),
            other => panic!("expected thinking chunk, got {other:?}"),
        }
    }

    #[test]
    fn codex_streaming_response_completed_emits_reasoning_before_done() {
        let state = Arc::new(Mutex::new(HashMap::<usize, CodexToolUseState>::new()));
        let chunk = br#"data: {"type":"response.completed","response":{"output":[{"type":"reasoning","summary":[{"text":"why"}],"content":[{"type":"reasoning_text","text":" because"},{"type":"text","text":" details"}],"encrypted_content":"abc"}]}}

"#;

        let events = codex_parse_stream_chunk_with_state(chunk, &state).unwrap();
        assert_eq!(events.len(), 2);
        match &events[0] {
            StreamChunk::Thinking(text) => assert_eq!(text, "why because details"),
            other => panic!("expected thinking chunk, got {other:?}"),
        }
        match &events[1] {
            StreamChunk::Done { finish_reason } => assert_eq!(*finish_reason, FinishReason::Stop),
            other => panic!("expected done chunk, got {other:?}"),
        }
    }

    #[test]
    fn codex_streaming_skips_output_item_done_reasoning_duplicate_after_deltas() {
        let state = Arc::new(Mutex::new(HashMap::<usize, CodexToolUseState>::new()));
        let first_delta = br#"data: {"type":"response.reasoning_summary_text.delta","delta":"**Acknowledging the question**\n\nI'm answering "}

"#;
        let second_delta = br#"data: {"type":"response.reasoning_summary_text.delta","delta":"with consideration!"}

"#;
        let output_item_done = br#"data: {"type":"response.output_item.done","item":{"type":"reasoning","summary":[{"text":"**Acknowledging the question**\n\nI'm answering with consideration!"}]}}

"#;

        let first_events = codex_parse_stream_chunk_with_state(first_delta, &state).unwrap();
        assert_eq!(first_events.len(), 1);
        match &first_events[0] {
            StreamChunk::Thinking(text) => {
                assert_eq!(text, "**Acknowledging the question**\n\nI'm answering ")
            }
            other => panic!("expected thinking chunk, got {other:?}"),
        }

        let second_events = codex_parse_stream_chunk_with_state(second_delta, &state).unwrap();
        assert_eq!(second_events.len(), 1);
        match &second_events[0] {
            StreamChunk::Thinking(text) => assert_eq!(text, "with consideration!"),
            other => panic!("expected thinking chunk, got {other:?}"),
        }

        let done_events = codex_parse_stream_chunk_with_state(output_item_done, &state).unwrap();
        assert!(done_events.is_empty(), "unexpected events: {done_events:?}");
    }

    #[test]
    fn codex_streaming_skips_completed_reasoning_duplicate_after_deltas() {
        let state = Arc::new(Mutex::new(HashMap::<usize, CodexToolUseState>::new()));
        let delta_chunk = br#"data: {"type":"response.reasoning_summary_text.delta","delta":"**Acknowledging the question**\n\nI'm answering "}

data: {"type":"response.reasoning_summary_text.delta","delta":"with consideration!"}

"#;
        let completed_chunk = br#"data: {"type":"response.completed","response":{"output":[{"type":"reasoning","summary":[{"text":"**Acknowledging the question**\n\nI'm answering with consideration!"}]}],"usage":{"input_tokens":10,"output_tokens":6,"input_tokens_details":{"cached_tokens":2},"output_tokens_details":{"reasoning_tokens":4}}}}

"#;

        let delta_events = codex_parse_stream_chunk_with_state(delta_chunk, &state).unwrap();
        assert_eq!(delta_events.len(), 2);
        match &delta_events[0] {
            StreamChunk::Thinking(text) => {
                assert_eq!(text, "**Acknowledging the question**\n\nI'm answering ")
            }
            other => panic!("expected thinking chunk, got {other:?}"),
        }
        match &delta_events[1] {
            StreamChunk::Thinking(text) => assert_eq!(text, "with consideration!"),
            other => panic!("expected thinking chunk, got {other:?}"),
        }

        let completed_events =
            codex_parse_stream_chunk_with_state(completed_chunk, &state).unwrap();
        assert_eq!(completed_events.len(), 2);
        match &completed_events[0] {
            StreamChunk::Usage(usage) => {
                assert_eq!(usage.input_tokens, 8);
                assert_eq!(usage.output_tokens, 2);
                assert_eq!(usage.reasoning_tokens, 4);
                assert_eq!(usage.cache_read, 2);
            }
            other => panic!("expected usage chunk, got {other:?}"),
        }
        match &completed_events[1] {
            StreamChunk::Done { finish_reason } => assert_eq!(*finish_reason, FinishReason::Stop),
            other => panic!("expected done chunk, got {other:?}"),
        }
    }

    #[test]
    fn codex_streaming_emits_completed_reasoning_suffix_after_deltas() {
        let state = Arc::new(Mutex::new(HashMap::<usize, CodexToolUseState>::new()));
        let delta_chunk =
            br#"data: {"type":"response.reasoning_summary_text.delta","delta":"first part"}

"#;
        let completed_chunk = br#"data: {"type":"response.completed","response":{"output":[{"type":"reasoning","summary":[{"text":"first part plus final"}]}],"usage":{"input_tokens":7,"output_tokens":5}}}

"#;

        let delta_events = codex_parse_stream_chunk_with_state(delta_chunk, &state).unwrap();
        assert_eq!(delta_events.len(), 1);
        match &delta_events[0] {
            StreamChunk::Thinking(text) => assert_eq!(text, "first part"),
            other => panic!("expected thinking chunk, got {other:?}"),
        }

        let completed_events =
            codex_parse_stream_chunk_with_state(completed_chunk, &state).unwrap();
        assert_eq!(completed_events.len(), 3);
        match &completed_events[0] {
            StreamChunk::Thinking(text) => assert_eq!(text, " plus final"),
            other => panic!("expected thinking chunk, got {other:?}"),
        }
        match &completed_events[1] {
            StreamChunk::Usage(usage) => {
                assert_eq!(usage.input_tokens, 7);
                assert_eq!(usage.output_tokens, 5);
                assert_eq!(usage.reasoning_tokens, 0);
                assert_eq!(usage.cache_read, 0);
            }
            other => panic!("expected usage chunk, got {other:?}"),
        }
        match &completed_events[2] {
            StreamChunk::Done { finish_reason } => assert_eq!(*finish_reason, FinishReason::Stop),
            other => panic!("expected done chunk, got {other:?}"),
        }
    }

    #[test]
    fn codex_streaming_skips_completed_reasoning_duplicate_after_output_item_done() {
        let state = Arc::new(Mutex::new(HashMap::<usize, CodexToolUseState>::new()));
        let chunk = br#"data: {"type":"response.output_item.done","item":{"type":"reasoning","summary":[{"text":"why"}]}}

data: {"type":"response.completed","response":{"output":[{"type":"reasoning","summary":[{"text":"why"}]}]}}

"#;

        let events = codex_parse_stream_chunk_with_state(chunk, &state).unwrap();
        assert_eq!(events.len(), 2);
        match &events[0] {
            StreamChunk::Thinking(text) => assert_eq!(text, "why"),
            other => panic!("expected thinking chunk, got {other:?}"),
        }
        match &events[1] {
            StreamChunk::Done { finish_reason } => assert_eq!(*finish_reason, FinishReason::Stop),
            other => panic!("expected done chunk, got {other:?}"),
        }
    }

    #[test]
    fn codex_streaming_keeps_distinct_completed_reasoning() {
        let state = Arc::new(Mutex::new(HashMap::<usize, CodexToolUseState>::new()));
        let chunk = br#"data: {"type":"response.output_item.done","item":{"type":"reasoning","summary":[{"text":"why"}]}}

data: {"type":"response.completed","response":{"output":[{"type":"reasoning","summary":[{"text":"because"}]}]}}

"#;

        let events = codex_parse_stream_chunk_with_state(chunk, &state).unwrap();
        assert_eq!(events.len(), 3);
        match &events[0] {
            StreamChunk::Thinking(text) => assert_eq!(text, "why"),
            other => panic!("expected thinking chunk, got {other:?}"),
        }
        match &events[1] {
            StreamChunk::Thinking(text) => assert_eq!(text, "because"),
            other => panic!("expected thinking chunk, got {other:?}"),
        }
        match &events[2] {
            StreamChunk::Done { finish_reason } => assert_eq!(*finish_reason, FinishReason::Stop),
            other => panic!("expected done chunk, got {other:?}"),
        }
    }

    #[test]
    fn codex_streaming_clears_completed_reasoning_between_requests() {
        let state = Arc::new(Mutex::new(HashMap::<usize, CodexToolUseState>::new()));
        let chunk = br#"data: {"type":"response.completed","response":{"output":[{"type":"reasoning","summary":[{"text":"same final thought"}]}]}}

"#;

        let first_events = codex_parse_stream_chunk_with_state(chunk, &state).unwrap();
        assert_eq!(first_events.len(), 2);
        match &first_events[0] {
            StreamChunk::Thinking(text) => assert_eq!(text, "same final thought"),
            other => panic!("expected thinking chunk, got {other:?}"),
        }
        match &first_events[1] {
            StreamChunk::Done { finish_reason } => assert_eq!(*finish_reason, FinishReason::Stop),
            other => panic!("expected done chunk, got {other:?}"),
        }

        let second_events = codex_parse_stream_chunk_with_state(chunk, &state).unwrap();
        assert_eq!(second_events.len(), 2);
        match &second_events[0] {
            StreamChunk::Thinking(text) => assert_eq!(text, "same final thought"),
            other => panic!("expected thinking chunk, got {other:?}"),
        }
        match &second_events[1] {
            StreamChunk::Done { finish_reason } => assert_eq!(*finish_reason, FinishReason::Stop),
            other => panic!("expected done chunk, got {other:?}"),
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
