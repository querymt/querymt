//! Unit tests: support.

use crate::config::Anthropic;
use http::Response;
use querymt::chat::http::HTTPChatProvider;
use querymt::chat::{
    ChatCall, ChatMessage, ChatOutput, ChatOutputItem, GenerationOptions, INVALID_INPUT_EXTENSION,
    ReasoningEffort, StructuredStreamEvent, Tool,
};
use serde_json::Value;

/// Build a minimal `Anthropic` instance for use in unit tests.
pub(crate) fn test_anthropic(api_key: &str) -> Anthropic {
    Anthropic {
        api_key: api_key.to_string(),
        auth_type: None,
        claude_code_version: None,
        model: "claude-3-7-sonnet-20250219".to_string(),
        max_tokens: 100,
        temperature: Some(1.0),
        timeout_seconds: None,
        system: None,
        stream: None,
        top_p: None,
        top_k: None,
        tools: None,
        tool_choice: None,
        reasoning_effort: None,
        reasoning_budget_tokens: None,
        adaptive_thinking: None,
        strict_tools: None,
        key_resolver: None,
    }
}

pub(crate) fn test_tool(name: &str) -> Tool {
    Tool {
        tool_type: "function".to_string(),
        function: querymt::chat::FunctionTool {
            name: name.to_string(),
            description: "test tool".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            strict: None,
        },
    }
}

/// Feed a sequence of raw SSE lines through a fresh stream parser and
/// collect all emitted `StreamChunk`s.
pub(crate) fn collect_chunks(
    anthropic: &Anthropic,
    lines: &[&str],
) -> Vec<querymt::chat::StreamChunk> {
    let mut parser = anthropic
        .chat_stream_parser(serde_json::Value::Null)
        .expect("stream parser should initialize");
    let mut out = Vec::new();
    for line in lines {
        let bytes = line.as_bytes();
        let parsed = parser.parse_chunk(bytes).unwrap();
        out.extend(parsed);
    }
    out
}

pub(crate) fn delta(index: usize, json: &str) -> String {
    let partial = serde_json::to_string(json).unwrap();
    format!(
        r#"data: {{"type":"content_block_delta","index":{index},"delta":{{"type":"input_json_delta","partial_json":{partial}}}}}"#
    )
}

/// `(call id, arguments, marked invalid)` for each completion, in order.
pub(crate) fn completions(chunks: &[querymt::chat::StreamChunk]) -> Vec<(String, String, bool)> {
    chunks
        .iter()
        .filter_map(|c| match c {
            querymt::chat::StreamChunk::Structured(StructuredStreamEvent::ItemCompleted {
                item: ChatOutputItem::FunctionCall(call),
                ..
            }) => Some((
                call.call_id.clone(),
                call.arguments.clone(),
                call.extensions.contains_key(INVALID_INPUT_EXTENSION),
            )),
            _ => None,
        })
        .collect()
}

/// Names of function calls announced by `ItemStarted` events.
pub(crate) fn started_call_names(chunks: &[querymt::chat::StreamChunk]) -> Vec<String> {
    chunks
        .iter()
        .filter_map(|c| match c {
            querymt::chat::StreamChunk::Structured(StructuredStreamEvent::ItemStarted {
                item: ChatOutputItem::FunctionCall(call),
                ..
            }) => Some(call.name.clone()),
            _ => None,
        })
        .collect()
}

/// Push every chunk through a fresh accumulator and finalize it.
pub(crate) fn accumulate(chunks: &[querymt::chat::StreamChunk]) -> querymt::chat::ChatStreamFinish {
    let mut acc = querymt::chat::ChatStreamAccumulator::new();
    for chunk in chunks {
        acc.push(chunk).unwrap();
    }
    acc.finish()
}

pub(crate) fn terminal_index(chunks: &[querymt::chat::StreamChunk]) -> usize {
    chunks
        .iter()
        .position(|c| {
            matches!(
                c,
                querymt::chat::StreamChunk::Structured(
                    StructuredStreamEvent::ResponseTerminal { .. }
                )
            )
        })
        .expect("terminal event")
}

pub(crate) fn anthropic_for(
    model: &str,
    effort: Option<ReasoningEffort>,
    max_tokens: u32,
) -> Anthropic {
    let mut a = test_anthropic("sk-ant-api03-test");
    a.model = model.to_string();
    a.reasoning_effort = effort;
    a.max_tokens = max_tokens;
    a.temperature = None;
    a
}

pub(crate) fn body_of(a: &Anthropic, messages: &[ChatMessage]) -> Value {
    let prepared = a
        .prepare_chat_request(&ChatCall::new(messages), false)
        .expect("request builds");
    serde_json::from_slice(prepared.request.body()).expect("valid JSON")
}

pub(crate) fn options_body(a: &Anthropic, options: &GenerationOptions, stream: bool) -> Value {
    let prepared = a
        .prepare_chat_request(&ChatCall::new(&user_msgs()).options(*options), stream)
        .expect("request builds");
    serde_json::from_slice(prepared.request.body()).expect("valid JSON")
}

pub(crate) fn user_msgs() -> Vec<ChatMessage> {
    vec![ChatMessage::user().text("hi").build()]
}

pub(crate) fn body(a: &Anthropic) -> Value {
    body_of(a, &user_msgs())
}

pub(crate) fn replay_assistant_content(items: Vec<ChatOutputItem>) -> Vec<Value> {
    let output = ChatOutput {
        items,
        ..ChatOutput::default()
    };
    let messages = vec![
        ChatMessage::user().text("go").build(),
        ChatMessage::from_assistant_output(output),
    ];
    let a = anthropic_for("claude-sonnet-5-5", None, 32_000);
    let b = body_of(&a, &messages);
    b["messages"][1]["content"].as_array().unwrap().clone()
}

pub(crate) static CAPTURED_WARNINGS: std::sync::Mutex<Vec<String>> =
    std::sync::Mutex::new(Vec::new());

pub(crate) struct CaptureLogger;
impl log::Log for CaptureLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
    }
    fn log(&self, record: &log::Record) {
        if record.level() == log::Level::Warn {
            CAPTURED_WARNINGS
                .lock()
                .unwrap()
                .push(record.args().to_string());
        }
    }
    fn flush(&self) {}
}

pub(crate) fn sse(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|l| format!("data: {l}\n")).collect()
}

pub(crate) fn stream_output(lines: &[&str]) -> (ChatOutput, Vec<querymt::chat::StreamChunk>) {
    let a = anthropic_for("claude-sonnet-5-5", None, 32_000);
    let data = sse(lines);
    let refs: Vec<&str> = data.iter().map(String::as_str).collect();
    let chunks = collect_chunks(&a, &refs);
    let mut acc = querymt::chat::ChatStreamAccumulator::new();
    for c in &chunks {
        acc.push(c).unwrap();
    }
    (acc.output(), chunks)
}

pub(crate) fn parse_response(json: Value) -> ChatOutput {
    let a = anthropic_for("claude-sonnet-5-5", None, 32_000);
    let resp = Response::builder()
        .status(200)
        .body(serde_json::to_vec(&json).unwrap())
        .unwrap();
    a.parse_chat(resp, serde_json::Value::Null)
        .expect("response parses")
}
