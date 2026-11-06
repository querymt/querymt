//! Session history lookup tool.
//!
//! Compaction and pruning remove content from the model's context but not from
//! the durable session history. This tool lets the agent search that history
//! and read a cleared tool result back on demand.

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool, ToolResultPart};
use serde_json::{Value, json};

use crate::agent::utils::{render_prompt_for_display, truncate_to_bytes};
use crate::model::{AgentMessage, MessagePart};
use crate::tools::{Tool as ToolTrait, ToolContext, ToolError};

const DEFAULT_SEARCH_LIMIT: usize = 20;
const MAX_SEARCH_LIMIT: usize = 100;
const DEFAULT_READ_LINES: usize = 400;
const SNIPPET_CONTEXT_BYTES: usize = 120;
const MAX_ARGUMENTS_BYTES: usize = 500;

/// Built-in tool that searches session history and reads back cleared tool results.
pub struct SessionHistoryTool;

impl Default for SessionHistoryTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionHistoryTool {
    /// Create a new `SessionHistoryTool`.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for SessionHistoryTool {
    fn name(&self) -> &str {
        "session_history"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Look up earlier parts of this session that were compacted or cleared from your context. \
Provide `query` to search past user prompts, assistant output, compaction summaries and tool results (case-insensitive substring, newest matches first; each match shows its call_id when it is a tool result). \
Provide `call_id` to read the full original output of one earlier tool call, including results shown as '[Old tool result content cleared]'. \
Exactly one of `query` or `call_id` is required."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Text to search for in the session history."
                        },
                        "call_id": {
                            "type": "string",
                            "description": "Tool call ID whose full output should be returned."
                        },
                        "offset": {
                            "type": "integer",
                            "description": "With `call_id`: 0-based line offset into the output.",
                            "default": 0,
                            "minimum": 0
                        },
                        "limit": {
                            "type": "integer",
                            "description": "With `query`: maximum matches (default 20, max 100). With `call_id`: maximum lines (default 400).",
                            "minimum": 1
                        }
                    },
                    "required": []
                }),
                strict: None,
            },
        }
    }

    fn truncation_hint(&self) -> Option<&'static str> {
        Some(
            "TIP: Use a narrower query, or offset/limit with call_id, to page through large results.",
        )
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<ToolResultPart>, ToolError> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .filter(|q| !q.is_empty());
        let call_id = args
            .get("call_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| usize::try_from(n).unwrap_or(usize::MAX));
        if limit == Some(0) {
            return Err(ToolError::InvalidRequest("limit must be >= 1".into()));
        }
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .map_or(0, |n| usize::try_from(n).unwrap_or(usize::MAX));

        if query.is_some() == call_id.is_some() {
            return Err(ToolError::InvalidRequest(
                "provide exactly one of `query` or `call_id`".into(),
            ));
        }

        let store = context
            .session_store()
            .ok_or_else(|| ToolError::ProviderError("Session store not available".into()))?;
        let session_id = context
            .session_public_id()
            .ok_or_else(|| ToolError::InvalidRequest("No session context available".into()))?;
        let messages = store
            .get_history(&session_id)
            .await
            .map_err(|e| ToolError::ProviderError(format!("Failed to load history: {e}")))?;

        let text = match (query, call_id) {
            (Some(query), _) => search_history(
                &messages,
                query,
                limit.unwrap_or(DEFAULT_SEARCH_LIMIT).min(MAX_SEARCH_LIMIT),
            ),
            (None, Some(call_id)) => read_tool_result(
                &messages,
                call_id,
                offset,
                limit.unwrap_or(DEFAULT_READ_LINES),
            )?,
            (None, None) => unreachable!("validated above"),
        };
        Ok(vec![ToolResultPart::text(text)])
    }
}

/// Concatenate the text parts of a tool result, one per line.
fn tool_result_text(content: &[ToolResultPart]) -> String {
    content
        .iter()
        .filter_map(ToolResultPart::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Largest char boundary of `s` that is `<= idx`.
fn floor_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Smallest char boundary of `s` that is `>= idx`.
fn ceil_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// A one-line excerpt around the first case-insensitive match, if any.
fn snippet(text: &str, query_lower: &str) -> Option<String> {
    let lowered = text.to_lowercase();
    let pos = lowered.find(query_lower)?;
    // Lowercasing can change byte lengths; only slice the original when the
    // offsets are still valid for it.
    let source = if lowered.len() == text.len() {
        text
    } else {
        lowered.as_str()
    };
    let start = floor_boundary(source, pos.saturating_sub(SNIPPET_CONTEXT_BYTES));
    let end = ceil_boundary(source, pos + query_lower.len() + SNIPPET_CONTEXT_BYTES);
    let mut out = source[start..end].replace(['\n', '\r'], " ");
    if start > 0 {
        out.insert_str(0, "...");
    }
    if end < source.len() {
        out.push_str("...");
    }
    Some(out)
}

/// Lowercase role name used in search result labels.
fn role_label(message: &AgentMessage) -> String {
    format!("{:?}", message.role).to_lowercase()
}

/// Case-insensitive substring search over history, newest matches first.
///
/// Earlier `session_history` results are skipped so repeated searches do not
/// match (and crowd out results with) their own prior output; they stay
/// readable by `call_id` via [`read_tool_result`].
fn search_history(messages: &[AgentMessage], query: &str, limit: usize) -> String {
    let query_lower = query.to_lowercase();
    let mut matches: Vec<String> = Vec::new();
    let mut total = 0usize;

    for message in messages.iter().rev() {
        for part in message.parts.iter().rev() {
            let (kind, label, haystack) = match part {
                MessagePart::Prompt { blocks } | MessagePart::Steering { blocks, .. } => (
                    "user_prompt",
                    String::new(),
                    render_prompt_for_display(blocks),
                ),
                MessagePart::Text { content } => ("text", String::new(), content.clone()),
                MessagePart::Compaction { summary, .. } => {
                    ("compaction_summary", String::new(), summary.clone())
                }
                MessagePart::Output { output } => {
                    ("assistant_output", String::new(), output.estimate_text())
                }
                MessagePart::ToolResult {
                    call_id,
                    content,
                    tool_name,
                    tool_arguments,
                    compacted_at,
                    ..
                } => {
                    if tool_name.as_deref() == Some("session_history") {
                        continue;
                    }
                    let name = tool_name.as_deref().unwrap_or("unknown");
                    let cleared = if compacted_at.is_some() {
                        " cleared_from_context"
                    } else {
                        ""
                    };
                    (
                        "tool_result",
                        format!(" call_id={call_id} tool={name}{cleared}"),
                        format!(
                            "{name}\n{}\n{}",
                            tool_arguments.as_deref().unwrap_or_default(),
                            tool_result_text(content)
                        ),
                    )
                }
                _ => continue,
            };
            let Some(excerpt) = snippet(&haystack, &query_lower) else {
                continue;
            };
            total += 1;
            if matches.len() < limit {
                matches.push(format!(
                    "- [{kind} role={}{label} message={}] {excerpt}",
                    role_label(message),
                    message.id
                ));
            }
        }
    }

    if total == 0 {
        return format!("No matches for {query:?} in this session's history.");
    }
    let mut out = format!(
        "{total} match(es) for {query:?}, showing {} (newest first):\n",
        matches.len()
    );
    out.push_str(&matches.join("\n"));
    if total > matches.len() {
        out.push_str("\n(more matches omitted; refine the query or raise `limit`)");
    }
    out
}

/// Render one tool result by `call_id`, paged by line `offset`/`limit`.
fn read_tool_result(
    messages: &[AgentMessage],
    call_id: &str,
    offset: usize,
    limit: usize,
) -> Result<String, ToolError> {
    let found = messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .find_map(|part| match part {
            MessagePart::ToolResult {
                call_id: id,
                content,
                is_error,
                tool_name,
                tool_arguments,
                compacted_at,
            } if id == call_id => {
                Some((content, *is_error, tool_name, tool_arguments, compacted_at))
            }
            _ => None,
        });
    let Some((content, is_error, tool_name, tool_arguments, compacted_at)) = found else {
        return Err(ToolError::InvalidRequest(format!(
            "no tool result with call_id {call_id:?} in this session; use `query` to find call IDs"
        )));
    };

    let text = tool_result_text(content);
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);

    let mut out = format!(
        "call_id: {call_id}\ntool: {}\narguments: {}\nstatus: {}{}\nlines: {}-{} of {total}\n---\n",
        tool_name.as_deref().unwrap_or("unknown"),
        truncate_to_bytes(
            tool_arguments.as_deref().unwrap_or_default(),
            MAX_ARGUMENTS_BYTES
        ),
        if compacted_at.is_some() {
            "cleared from your context"
        } else {
            "still in your context"
        },
        if is_error { ", error result" } else { "" },
        if total == 0 { 0 } else { start + 1 },
        end,
    );
    out.push_str(&lines[start..end].join("\n"));
    if end < total {
        out.push_str(&format!("\n[{} more lines; use offset={end}]", total - end));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::session::store::SessionStore;
    use crate::tools::AgentToolContext;
    use querymt::chat::ChatRole;
    use std::sync::Arc;

    fn tool_result_message(
        session_id: &str,
        call_id: &str,
        text: &str,
        cleared: bool,
    ) -> AgentMessage {
        AgentMessage {
            id: format!("msg-{call_id}"),
            session_id: session_id.to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::ToolResult {
                call_id: call_id.to_string(),
                content: vec![ToolResultPart::text(text)],
                is_error: false,
                tool_name: Some("shell".to_string()),
                tool_arguments: Some(r#"{"command":"cargo test"}"#.to_string()),
                compacted_at: cleared.then_some(1),
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        }
    }

    async fn context_with_history(texts: &[(&str, &str, bool)]) -> AgentToolContext {
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let session = storage
            .create_session(Some("history".to_string()), None, None, None)
            .await
            .unwrap();
        for (call_id, text, cleared) in texts {
            storage
                .add_message(
                    &session.public_id,
                    tool_result_message(&session.public_id, call_id, text, *cleared),
                )
                .await
                .unwrap();
        }
        AgentToolContext::basic(session.public_id, None).with_session_store(storage)
    }

    fn text_of(parts: Vec<ToolResultPart>) -> String {
        parts
            .iter()
            .filter_map(ToolResultPart::as_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn search_finds_cleared_result_and_call_id_reads_full_output() {
        let context = context_with_history(&[
            (
                "call-1",
                "line one\nthe NEEDLE failure is here\nline three",
                true,
            ),
            ("call-2", "unrelated", false),
        ])
        .await;
        let tool = SessionHistoryTool::new();

        let found = text_of(
            tool.call(json!({"query": "needle"}), &context)
                .await
                .unwrap(),
        );
        assert!(found.contains("call_id=call-1"), "{found}");
        assert!(found.contains("cleared_from_context"), "{found}");
        assert!(!found.contains("call-2"), "{found}");

        let read = text_of(
            tool.call(json!({"call_id": "call-1", "limit": 2}), &context)
                .await
                .unwrap(),
        );
        assert!(read.contains("cleared from your context"), "{read}");
        assert!(read.contains("the NEEDLE failure is here"), "{read}");
        assert!(read.contains("1 more lines; use offset=2"), "{read}");
        assert!(!read.contains("line three"), "{read}");
    }

    #[test]
    fn search_skips_session_history_results() {
        let mut own = tool_result_message("s", "call-own", "echoed NEEDLE", false);
        if let MessagePart::ToolResult { tool_name, .. } = &mut own.parts[0] {
            *tool_name = Some("session_history".to_string());
        }
        let real = tool_result_message("s", "call-real", "real NEEDLE", false);
        let out = search_history(&[real, own.clone()], "needle", 10);
        assert!(out.contains("call-real"), "{out}");
        assert!(!out.contains("call-own"), "{out}");
        assert!(out.starts_with("1 match(es)"), "{out}");
        // Still readable by call_id.
        assert!(read_tool_result(&[own], "call-own", 0, 10).is_ok());
    }

    #[tokio::test]
    async fn requires_exactly_one_selector_and_known_call_id() {
        let context = context_with_history(&[("call-1", "x", false)]).await;
        let tool = SessionHistoryTool::new();

        assert!(tool.call(json!({}), &context).await.is_err());
        assert!(
            tool.call(json!({"query": "x", "call_id": "call-1"}), &context)
                .await
                .is_err()
        );
        assert!(
            tool.call(json!({"call_id": "missing"}), &context)
                .await
                .is_err()
        );
    }
}
