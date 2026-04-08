//! SFT (Supervised Fine-Tuning) training data export.
//!
//! Exports agent session data as JSONL files suitable for fine-tuning LLMs.
//! Supports two output formats:
//!
//! - **OpenAI Chat**: `{"messages": [{role, content, tool_calls?, tool_call_id?}]}`
//! - **ShareGPT**: `{"conversations": [{from, value}]}`
//!
//! The exporter reads sessions one at a time via the event journal, materializes
//! turns using [`super::turns::materialize_turns`], and writes each training
//! example as a single JSONL line. Memory usage is proportional to one session
//! at a time, not the total dataset.

use crate::events::AgentEvent;
use crate::export::turns::{Turn, materialize_turns};
use crate::session::backend::StorageBackend;
use crate::session::store::SessionStore;
use serde::Serialize;
use std::io::{self, Write};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Output format for SFT training data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SftFormat {
    /// OpenAI fine-tuning format: `{"messages": [...]}`
    OpenAiChat,
    /// ShareGPT / unsloth format: `{"conversations": [...]}`
    ShareGpt,
}

impl SftFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai" | "openai_chat" => Some(Self::OpenAiChat),
            "sharegpt" => Some(Self::ShareGpt),
            _ => None,
        }
    }
}

/// Quality filters for session selection.
#[derive(Debug, Clone)]
pub struct SessionFilter {
    /// Minimum number of LLM turns to include a session (default: 1).
    pub min_turns: usize,
    /// Maximum tool error rate (0.0–1.0) before excluding a session (default: 1.0 = no filter).
    pub max_tool_error_rate: f32,
    /// Only include sessions that used one of these models. `None` = all models.
    pub source_models: Option<Vec<String>>,
    /// Exclude sessions that contain Error events.
    pub exclude_errored: bool,
}

impl Default for SessionFilter {
    fn default() -> Self {
        Self {
            min_turns: 1,
            max_tool_error_rate: 1.0,
            source_models: None,
            exclude_errored: false,
        }
    }
}

/// Options for SFT export.
#[derive(Debug, Clone)]
pub struct SftExportOptions {
    /// Output format.
    pub format: SftFormat,
    /// Quality filters.
    pub filter: SessionFilter,
    /// Replace home directory paths with a placeholder.
    pub scrub_paths: bool,
    /// Replacement string for scrubbed paths (default: "/workspace").
    pub path_replacement: String,
    /// Maximum number of context messages per training example (sliding window).
    /// `None` = include full history (may exceed model context during training).
    pub max_context_messages: Option<usize>,
    /// Include thinking/reasoning content in training examples.
    pub include_thinking: bool,
    /// Include tool result content. When false, tool results are replaced
    /// with a short placeholder to reduce data size.
    pub include_tool_results: bool,
}

impl Default for SftExportOptions {
    fn default() -> Self {
        Self {
            format: SftFormat::OpenAiChat,
            filter: SessionFilter::default(),
            scrub_paths: false,
            path_replacement: "/workspace".to_string(),
            max_context_messages: Some(40),
            include_thinking: false,
            include_tool_results: true,
        }
    }
}

/// Statistics from an SFT export run.
#[derive(Debug, Clone, Default)]
pub struct SftExportStats {
    /// Total sessions inspected.
    pub sessions_total: usize,
    /// Sessions that passed filters and were exported.
    pub sessions_exported: usize,
    /// Sessions skipped by filters.
    pub sessions_skipped: usize,
    /// Total training examples written.
    pub training_examples: usize,
    /// Total bytes written.
    pub total_bytes: u64,
}

// ---------------------------------------------------------------------------
// Core export logic
// ---------------------------------------------------------------------------

/// Export a single session's turns as JSONL training examples.
///
/// Each assistant turn (with its context window) becomes one JSONL line.
/// Returns the number of training examples written.
pub fn write_session_sft(
    turns: &[Turn],
    system_prompt: Option<&str>,
    options: &SftExportOptions,
    writer: &mut dyn Write,
) -> io::Result<usize> {
    if turns.is_empty() {
        return Ok(0);
    }

    let mut examples_written = 0;

    // Build a running conversation history for context windowing.
    // Each turn produces messages that are appended to this buffer.
    let mut conversation: Vec<SftMessage> = Vec::new();

    // Add system prompt once at the start
    if let Some(sys) = system_prompt {
        let content = if options.scrub_paths {
            scrub_paths(sys, &options.path_replacement)
        } else {
            sys.to_string()
        };
        conversation.push(SftMessage {
            role: "system".to_string(),
            content,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
    }

    for turn in turns {
        // Add user message (if present)
        if let Some(user) = &turn.user_content {
            let content = if options.scrub_paths {
                scrub_paths(user, &options.path_replacement)
            } else {
                user.clone()
            };
            conversation.push(SftMessage {
                role: "user".to_string(),
                content,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }

        // Build assistant message
        let mut assistant_content = turn.assistant_content.clone();

        // Optionally prepend thinking
        if let Some(thinking) = &turn.thinking
            && options.include_thinking
        {
            assistant_content = format!("<thinking>\n{thinking}\n</thinking>\n{assistant_content}");
        }

        if options.scrub_paths {
            assistant_content = scrub_paths(&assistant_content, &options.path_replacement);
        }

        let tool_calls = if turn.tool_calls.is_empty() {
            None
        } else {
            Some(
                turn.tool_calls
                    .iter()
                    .map(|tc| SftToolCall {
                        id: tc.id.clone(),
                        r#type: "function".to_string(),
                        function: SftFunctionCall {
                            name: tc.name.clone(),
                            arguments: if options.scrub_paths {
                                scrub_paths(&tc.arguments, &options.path_replacement)
                            } else {
                                tc.arguments.clone()
                            },
                        },
                    })
                    .collect(),
            )
        };

        conversation.push(SftMessage {
            role: "assistant".to_string(),
            content: assistant_content,
            tool_calls,
            tool_call_id: None,
            name: None,
        });

        // Add tool results as separate "tool" role messages
        for tr in &turn.tool_results {
            let content = if options.include_tool_results {
                if options.scrub_paths {
                    scrub_paths(&tr.content, &options.path_replacement)
                } else {
                    tr.content.clone()
                }
            } else {
                "[tool result omitted]".to_string()
            };

            conversation.push(SftMessage {
                role: "tool".to_string(),
                content,
                tool_calls: None,
                tool_call_id: Some(tr.call_id.clone()),
                name: Some(tr.name.clone()),
            });
        }

        // Only emit a training example when the assistant gives a final response
        // (no tool calls = end of reasoning chain, or last turn in sequence)
        let is_final_response = turn.tool_calls.is_empty();
        let is_last_turn = std::ptr::eq(turn, turns.last().unwrap());

        if is_final_response || is_last_turn {
            // Apply context window
            let messages = apply_context_window(&conversation, options.max_context_messages);

            // Write as JSONL
            let bytes = match options.format {
                SftFormat::OpenAiChat => write_openai_chat(&messages, writer)?,
                SftFormat::ShareGpt => write_sharegpt(&messages, writer)?,
            };

            examples_written += 1;
            let _ = bytes; // stats tracked by caller via CountingWriter if needed
        }
    }

    Ok(examples_written)
}

/// Export all qualifying sessions from a storage backend.
///
/// Loads one session at a time to control memory. Writes JSONL to the
/// provided writer.
pub async fn export_all_sessions(
    storage: &dyn StorageBackend,
    options: &SftExportOptions,
    writer: &mut (dyn Write + Send),
) -> Result<SftExportStats, ExportError> {
    let session_store = storage.session_store();
    let event_journal = storage.event_journal();

    let sessions = session_store
        .list_sessions()
        .await
        .map_err(|e| ExportError::Storage(e.to_string()))?;

    let mut stats = SftExportStats {
        sessions_total: sessions.len(),
        ..Default::default()
    };

    // Resolve home dir once for path scrubbing
    let home_dir = if options.scrub_paths {
        dirs::home_dir().map(|p| p.to_string_lossy().to_string())
    } else {
        None
    };

    for session in &sessions {
        // Load events for this session
        let durable_events = event_journal
            .load_session_stream(&session.public_id, None, None)
            .await
            .map_err(|e| ExportError::Storage(e.to_string()))?;

        let events: Vec<AgentEvent> = durable_events.into_iter().map(AgentEvent::from).collect();

        let (turns, meta) = materialize_turns(&events);

        // Apply filters
        if !passes_filter(&turns, &events, &meta, &options.filter) {
            stats.sessions_skipped += 1;
            continue;
        }

        // Extract system prompt from session's LLM config
        let system_prompt =
            resolve_system_prompt(&session_store, session.llm_config_id, &meta).await;

        let system_prompt_str = system_prompt.as_deref();

        // If scrubbing, we wrap the options to include home dir
        let mut session_options = options.clone();
        if let Some(ref home) = home_dir {
            session_options.path_replacement = options.path_replacement.clone();
            // Store the home dir pattern for scrub_paths to use
            SCRUB_HOME.with(|cell| cell.replace(Some(home.clone())));
        }

        let examples = write_session_sft(&turns, system_prompt_str, &session_options, writer)
            .map_err(|e| ExportError::Io(e.to_string()))?;

        if examples > 0 {
            stats.sessions_exported += 1;
            stats.training_examples += examples;
        } else {
            stats.sessions_skipped += 1;
        }
    }

    // Clear thread-local
    if options.scrub_paths {
        SCRUB_HOME.with(|cell| cell.replace(None));
    }

    Ok(stats)
}

/// Compute export stats without writing any data.
pub async fn preview_export(
    storage: &dyn StorageBackend,
    options: &SftExportOptions,
) -> Result<SftExportStats, ExportError> {
    let session_store = storage.session_store();
    let event_journal = storage.event_journal();

    let sessions = session_store
        .list_sessions()
        .await
        .map_err(|e| ExportError::Storage(e.to_string()))?;

    let mut stats = SftExportStats {
        sessions_total: sessions.len(),
        ..Default::default()
    };

    for session in &sessions {
        let durable_events = event_journal
            .load_session_stream(&session.public_id, None, None)
            .await
            .map_err(|e| ExportError::Storage(e.to_string()))?;

        let events: Vec<AgentEvent> = durable_events.into_iter().map(AgentEvent::from).collect();
        let (turns, meta) = materialize_turns(&events);

        if !passes_filter(&turns, &events, &meta, &options.filter) {
            stats.sessions_skipped += 1;
            continue;
        }

        // Count final-response turns (same logic as write_session_sft)
        let example_count = turns
            .iter()
            .enumerate()
            .filter(|(i, t)| t.tool_calls.is_empty() || *i == turns.len() - 1)
            .count();

        if example_count > 0 {
            stats.sessions_exported += 1;
            stats.training_examples += example_count;
        } else {
            stats.sessions_skipped += 1;
        }
    }

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ExportError {
    Storage(String),
    Io(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::Storage(e) => write!(f, "Storage error: {}", e),
            ExportError::Io(e) => write!(f, "IO error: {}", e),
        }
    }
}

impl std::error::Error for ExportError {}

// ---------------------------------------------------------------------------
// Serialization types (not public — internal JSONL wire format)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SftMessage {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<SftToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Serialize)]
struct SftToolCall {
    id: String,
    r#type: String,
    function: SftFunctionCall,
}

#[derive(Serialize)]
struct SftFunctionCall {
    name: String,
    arguments: String,
}

// ShareGPT format
#[derive(Serialize)]
struct ShareGptExample {
    conversations: Vec<ShareGptMessage>,
}

#[derive(Serialize)]
struct ShareGptMessage {
    from: String,
    value: String,
}

// ---------------------------------------------------------------------------
// Format writers
// ---------------------------------------------------------------------------

/// Write a single training example in OpenAI chat JSONL format.
fn write_openai_chat(messages: &[&SftMessage], writer: &mut dyn Write) -> io::Result<usize> {
    let json = serde_json::to_string(&OpenAiChatBorrowed { messages }).map_err(io::Error::other)?;
    let bytes = json.len() + 1; // +1 for newline
    writeln!(writer, "{}", json)?;
    Ok(bytes)
}

#[derive(Serialize)]
struct OpenAiChatBorrowed<'a> {
    messages: &'a [&'a SftMessage],
}

/// Write a single training example in ShareGPT JSONL format.
fn write_sharegpt(messages: &[&SftMessage], writer: &mut dyn Write) -> io::Result<usize> {
    let conversations: Vec<ShareGptMessage> = messages
        .iter()
        .map(|m| {
            let from = match m.role.as_str() {
                "system" => "system",
                "user" => "human",
                "assistant" => "gpt",
                "tool" => "tool",
                other => other,
            };

            // For ShareGPT, fold tool_calls into the value text
            let mut value = m.content.clone();
            if let Some(ref calls) = m.tool_calls {
                for tc in calls {
                    value.push_str(&format!(
                        "\n<tool_call>\n{{\"name\": \"{}\", \"arguments\": {}}}\n</tool_call>",
                        tc.function.name, tc.function.arguments
                    ));
                }
            }

            ShareGptMessage {
                from: from.to_string(),
                value,
            }
        })
        .collect();

    let example = ShareGptExample { conversations };
    let json = serde_json::to_string(&example).map_err(io::Error::other)?;
    let bytes = json.len() + 1;
    writeln!(writer, "{}", json)?;
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Filtering
// ---------------------------------------------------------------------------

/// Check whether a session passes the configured filters.
fn passes_filter(
    turns: &[Turn],
    events: &[AgentEvent],
    meta: &crate::export::turns::SessionMeta,
    filter: &SessionFilter,
) -> bool {
    // Minimum turns
    if turns.len() < filter.min_turns {
        return false;
    }

    // Model filter
    if let Some(ref allowed_models) = filter.source_models {
        let session_model = meta
            .initial_model
            .as_deref()
            .or_else(|| turns.first().and_then(|t| t.model.as_deref()));
        match session_model {
            Some(model) => {
                if !allowed_models.iter().any(|m| m == model) {
                    return false;
                }
            }
            None => return false,
        }
    }

    // Error filter
    if filter.exclude_errored {
        let has_error = events
            .iter()
            .any(|e| matches!(e.kind, crate::events::AgentEventKind::Error { .. }));
        if has_error {
            return false;
        }
    }

    // Tool error rate
    if filter.max_tool_error_rate < 1.0 {
        let total_results: usize = turns.iter().map(|t| t.tool_results.len()).sum();
        let error_results: usize = turns
            .iter()
            .flat_map(|t| &t.tool_results)
            .filter(|r| r.is_error)
            .count();
        if total_results > 0 {
            let error_rate = error_results as f32 / total_results as f32;
            if error_rate > filter.max_tool_error_rate {
                return false;
            }
        }
    }

    true
}

// ---------------------------------------------------------------------------
// Path scrubbing
// ---------------------------------------------------------------------------

thread_local! {
    static SCRUB_HOME: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Replace home directory paths with a placeholder.
fn scrub_paths(text: &str, replacement: &str) -> String {
    SCRUB_HOME.with(|cell| {
        let borrow = cell.borrow();
        match borrow.as_ref() {
            Some(home) => text.replace(home.as_str(), replacement),
            None => {
                // Fallback: try to detect home dir
                if let Some(home) = dirs::home_dir() {
                    text.replace(&home.to_string_lossy().to_string(), replacement)
                } else {
                    text.to_string()
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Context windowing
// ---------------------------------------------------------------------------

/// Apply a sliding context window to the conversation.
///
/// Keeps the system message (if any) and the last `max` messages.
fn apply_context_window(messages: &[SftMessage], max: Option<usize>) -> Vec<&SftMessage> {
    let Some(max) = max else {
        return messages.iter().collect();
    };

    if messages.len() <= max {
        return messages.iter().collect();
    }

    let mut result = Vec::with_capacity(max + 1);

    // Always keep the system message if it's first
    let start = if messages.first().is_some_and(|m| m.role == "system") {
        result.push(&messages[0]);
        1
    } else {
        0
    };

    // Take the last `max` messages from the non-system portion
    let non_system = &messages[start..];
    let skip = non_system.len().saturating_sub(max);
    for msg in &non_system[skip..] {
        result.push(msg);
    }

    result
}

// ---------------------------------------------------------------------------
// System prompt resolution
// ---------------------------------------------------------------------------

/// Resolve system prompt from LLM config or session metadata.
async fn resolve_system_prompt(
    session_store: &std::sync::Arc<dyn SessionStore>,
    llm_config_id: Option<i64>,
    meta: &crate::export::turns::SessionMeta,
) -> Option<String> {
    // First try: LLM config stored in the database
    if let Some(config_id) = llm_config_id
        && let Ok(Some(config)) = session_store.get_llm_config(config_id).await
        && let Some(params) = &config.params
        && let Some(system_arr) = params.get("system").and_then(|v| v.as_array())
    {
        let prompt: String = system_arr
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !prompt.is_empty() {
            return Some(prompt);
        }
    }

    // Fallback: middleware-injected system prompts from events
    if !meta.system_prompts.is_empty() {
        return Some(meta.system_prompts.join("\n"));
    }

    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::turns::{TurnToolCall, TurnToolResult};

    fn make_turn(user: Option<&str>, assistant: &str) -> Turn {
        Turn {
            user_content: user.map(String::from),
            assistant_content: assistant.to_string(),
            thinking: None,
            tool_calls: vec![],
            tool_results: vec![],
            delegations: vec![],
            model: Some("test-model".to_string()),
            provider: Some("test-provider".to_string()),
            usage: None,
            cost_usd: None,
            finish_reason: None,
            timestamp: 1000,
        }
    }

    fn make_tool_turn(user: Option<&str>, assistant: &str) -> Turn {
        Turn {
            user_content: user.map(String::from),
            assistant_content: assistant.to_string(),
            thinking: None,
            tool_calls: vec![TurnToolCall {
                id: "call-1".to_string(),
                name: "read_tool".to_string(),
                arguments: r#"{"path":"/home/user/test.rs"}"#.to_string(),
            }],
            tool_results: vec![TurnToolResult {
                call_id: "call-1".to_string(),
                name: "read_tool".to_string(),
                content: "fn main() {}".to_string(),
                is_error: false,
            }],
            delegations: vec![],
            model: Some("test-model".to_string()),
            provider: Some("test-provider".to_string()),
            usage: None,
            cost_usd: None,
            finish_reason: None,
            timestamp: 1000,
        }
    }

    #[test]
    fn openai_chat_simple_turn() {
        let turns = vec![make_turn(Some("hello"), "hi there")];
        let mut buf = Vec::new();
        let options = SftExportOptions::default();

        let count =
            write_session_sft(&turns, Some("You are helpful."), &options, &mut buf).unwrap();

        assert_eq!(count, 1);
        let output = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert!(parsed["messages"].is_array());

        let msgs = parsed["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "hello");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "hi there");
    }

    #[test]
    fn sharegpt_simple_turn() {
        let turns = vec![make_turn(Some("hello"), "hi there")];
        let mut buf = Vec::new();
        let options = SftExportOptions {
            format: SftFormat::ShareGpt,
            ..SftExportOptions::default()
        };

        let count = write_session_sft(&turns, None, &options, &mut buf).unwrap();

        assert_eq!(count, 1);
        let output = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert!(parsed["conversations"].is_array());

        let convs = parsed["conversations"].as_array().unwrap();
        assert_eq!(convs[0]["from"], "human");
        assert_eq!(convs[1]["from"], "gpt");
    }

    #[test]
    fn openai_chat_with_tool_calls() {
        let turns = vec![
            make_tool_turn(Some("read test.rs"), ""),
            make_turn(None, "Here is the file content."),
        ];
        let mut buf = Vec::new();
        let options = SftExportOptions::default();

        let count = write_session_sft(&turns, None, &options, &mut buf).unwrap();

        // Should emit 1 example (the final response turn)
        assert_eq!(count, 1);
        let output = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        let msgs = parsed["messages"].as_array().unwrap();

        // Should include: user, assistant (with tool_calls), tool result, assistant (final)
        assert!(msgs.len() >= 4);

        // Find assistant message with tool_calls
        let tool_msg = msgs.iter().find(|m| m["tool_calls"].is_array()).unwrap();
        assert_eq!(tool_msg["role"], "assistant");
    }

    #[test]
    fn path_scrubbing() {
        SCRUB_HOME.with(|cell| cell.replace(Some("/home/user".to_string())));
        let result = scrub_paths("Reading /home/user/test.rs", "/workspace");
        assert_eq!(result, "Reading /workspace/test.rs");
        SCRUB_HOME.with(|cell| cell.replace(None));
    }

    #[test]
    fn context_window_preserves_system() {
        let msgs: Vec<SftMessage> = (0..10)
            .map(|i| SftMessage {
                role: if i == 0 {
                    "system"
                } else if i % 2 == 1 {
                    "user"
                } else {
                    "assistant"
                }
                .to_string(),
                content: format!("msg-{}", i),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            })
            .collect();

        let windowed = apply_context_window(&msgs, Some(4));
        // Should have system + last 4
        assert_eq!(windowed.len(), 5);
        assert_eq!(windowed[0].role, "system");
        assert_eq!(windowed[1].content, "msg-6");
    }

    #[test]
    fn empty_turns_produces_nothing() {
        let turns: Vec<Turn> = vec![];
        let mut buf = Vec::new();
        let options = SftExportOptions::default();
        let count = write_session_sft(&turns, None, &options, &mut buf).unwrap();
        assert_eq!(count, 0);
        assert!(buf.is_empty());
    }

    #[test]
    fn filter_min_turns() {
        let turns = vec![make_turn(Some("hi"), "hello")];
        let filter = SessionFilter {
            min_turns: 5,
            ..Default::default()
        };
        let meta = crate::export::turns::SessionMeta::default();
        assert!(!passes_filter(&turns, &[], &meta, &filter));
    }

    #[test]
    fn filter_model() {
        let turns = vec![make_turn(Some("hi"), "hello")];
        let filter = SessionFilter {
            source_models: Some(vec!["gpt-4".to_string()]),
            ..Default::default()
        };
        let meta = crate::export::turns::SessionMeta {
            initial_model: Some("claude-opus".to_string()),
            ..Default::default()
        };
        assert!(!passes_filter(&turns, &[], &meta, &filter));

        let meta2 = crate::export::turns::SessionMeta {
            initial_model: Some("gpt-4".to_string()),
            ..Default::default()
        };
        assert!(passes_filter(&turns, &[], &meta2, &filter));
    }
}
