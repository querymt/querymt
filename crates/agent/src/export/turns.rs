//! Shared turn materialization from agent event streams.
//!
//! Walks a sequence of [`AgentEvent`]s and produces a `Vec<Turn>` — a
//! structured, consumer-friendly representation of the conversation.
//! Both the ATIF exporter and the SFT exporter build on this.

use crate::events::{AgentEvent, AgentEventKind};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single materialized turn in the conversation.
///
/// Each turn corresponds to one LLM request/response cycle:
/// an optional user message, the assistant's text + tool calls,
/// tool results, and associated metadata.
#[derive(Debug, Clone)]
pub struct Turn {
    /// User content that preceded this LLM call (if any).
    pub user_content: Option<String>,
    /// Assistant text content (may be empty for tool-call-only responses).
    pub assistant_content: String,
    /// Reasoning/thinking content (if the model produced it).
    pub thinking: Option<String>,
    /// Tool calls made by the assistant in this turn.
    pub tool_calls: Vec<TurnToolCall>,
    /// Tool results returned to the assistant.
    pub tool_results: Vec<TurnToolResult>,
    /// Delegation completions within this turn.
    pub delegations: Vec<TurnDelegation>,
    /// Model that produced this response.
    pub model: Option<String>,
    /// Provider that produced this response.
    pub provider: Option<String>,
    /// Token usage for this turn.
    pub usage: Option<querymt::Usage>,
    /// Cost in USD for this turn.
    pub cost_usd: Option<f64>,
    /// Finish reason reported by the LLM.
    pub finish_reason: Option<querymt::chat::FinishReason>,
    /// Unix timestamp of the LLM response.
    pub timestamp: i64,
}

/// A tool call within a turn.
#[derive(Debug, Clone)]
pub struct TurnToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON string of arguments.
    pub arguments: String,
}

/// A tool result within a turn.
#[derive(Debug, Clone)]
pub struct TurnToolResult {
    pub call_id: String,
    pub name: String,
    pub content: String,
    pub is_error: bool,
}

/// A delegation completion within a turn.
#[derive(Debug, Clone)]
pub struct TurnDelegation {
    pub delegation_id: String,
    pub result: Option<String>,
}

/// Metadata extracted from the event stream alongside turns.
#[derive(Debug, Clone, Default)]
pub struct SessionMeta {
    /// System prompt injected by middleware.
    pub system_prompts: Vec<String>,
    /// Model active at session start (from first ProviderChanged).
    pub initial_model: Option<String>,
    /// Provider active at session start.
    pub initial_provider: Option<String>,
    /// Tool definitions available in the session.
    pub tool_definitions: Option<Vec<querymt::chat::Tool>>,
}

// ---------------------------------------------------------------------------
// Materialization
// ---------------------------------------------------------------------------

/// Extract structured turns and session metadata from a raw event stream.
///
/// Events must be ordered by sequence number (as returned by
/// [`EventJournal::load_session_stream`]).
pub fn materialize_turns(events: &[AgentEvent]) -> (Vec<Turn>, SessionMeta) {
    let mut turns = Vec::new();
    let mut meta = SessionMeta::default();

    // Tracking state
    let mut current_model: Option<String> = None;
    let mut current_provider: Option<String> = None;
    let mut pending_user_content: Option<String> = None;

    let mut i = 0;
    while i < events.len() {
        match &events[i].kind {
            // ── Metadata events ────────────────────────────────────────
            AgentEventKind::ProviderChanged {
                provider, model, ..
            } => {
                if meta.initial_model.is_none() {
                    meta.initial_model = Some(model.clone());
                    meta.initial_provider = Some(provider.clone());
                }
                current_model = Some(model.clone());
                current_provider = Some(provider.clone());
            }

            AgentEventKind::MiddlewareInjected { message } => {
                meta.system_prompts.push(message.clone());
            }

            AgentEventKind::ToolsAvailable { tools, .. } if meta.tool_definitions.is_none() => {
                meta.tool_definitions = Some(tools.clone());
            }

            // ── User messages ──────────────────────────────────────────
            AgentEventKind::PromptReceived { content, .. }
            | AgentEventKind::UserMessageStored { content } => {
                pending_user_content = Some(content.clone());
            }

            // ── LLM turn span ──────────────────────────────────────────
            AgentEventKind::LlmRequestStart { .. } => {
                if let Some(end_idx) = find_llm_request_end(events, i) {
                    let turn = extract_turn(
                        events,
                        i,
                        end_idx,
                        pending_user_content.take(),
                        current_model.clone(),
                        current_provider.clone(),
                    );
                    turns.push(turn);
                    i = end_idx; // will be incremented at loop end
                }
            }

            _ => {}
        }
        i += 1;
    }

    (turns, meta)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Find the `LlmRequestEnd` event that closes the span starting at `start_idx`.
fn find_llm_request_end(events: &[AgentEvent], start_idx: usize) -> Option<usize> {
    for (idx, event) in events.iter().enumerate().skip(start_idx + 1) {
        if matches!(event.kind, AgentEventKind::LlmRequestEnd { .. }) {
            return Some(idx);
        }
    }
    None
}

/// Extract a single [`Turn`] from events between `start_idx` (LlmRequestStart)
/// and `end_idx` (LlmRequestEnd) inclusive.
fn extract_turn(
    events: &[AgentEvent],
    start_idx: usize,
    end_idx: usize,
    user_content: Option<String>,
    model: Option<String>,
    provider: Option<String>,
) -> Turn {
    let end_event = &events[end_idx];

    // Extract usage/cost/finish_reason from LlmRequestEnd
    let (usage, cost_usd, finish_reason) = if let AgentEventKind::LlmRequestEnd {
        usage,
        cost_usd,
        finish_reason,
        ..
    } = &end_event.kind
    {
        (usage.clone(), *cost_usd, *finish_reason)
    } else {
        (None, None, None)
    };

    let mut assistant_content = String::new();
    let mut thinking: Option<String> = None;
    let mut tool_calls = Vec::new();
    let mut tool_results = Vec::new();
    let mut delegations = Vec::new();

    for event in &events[start_idx..=end_idx] {
        match &event.kind {
            AgentEventKind::AssistantMessageStored {
                content,
                thinking: think,
                ..
            } => {
                if !content.is_empty() {
                    assistant_content.push_str(content);
                }
                if let Some(t) = think {
                    thinking = Some(t.clone());
                }
            }

            AgentEventKind::ToolCallStart {
                tool_call_id,
                tool_name,
                arguments,
            } => {
                tool_calls.push(TurnToolCall {
                    id: tool_call_id.clone(),
                    name: tool_name.clone(),
                    arguments: arguments.clone(),
                });
            }

            AgentEventKind::ToolCallEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => {
                tool_results.push(TurnToolResult {
                    call_id: tool_call_id.clone(),
                    name: tool_name.clone(),
                    content: result.clone(),
                    is_error: *is_error,
                });
            }

            AgentEventKind::DelegationCompleted {
                delegation_id,
                result,
            } => {
                delegations.push(TurnDelegation {
                    delegation_id: delegation_id.clone(),
                    result: result.clone(),
                });
            }

            _ => {}
        }
    }

    Turn {
        user_content,
        assistant_content,
        thinking,
        tool_calls,
        tool_results,
        delegations,
        model,
        provider,
        usage,
        cost_usd,
        finish_reason,
        timestamp: end_event.timestamp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventOrigin, ExecutionMetrics};

    fn make_event(seq: i64, ts: i64, kind: AgentEventKind) -> AgentEvent {
        AgentEvent {
            seq,
            timestamp: ts,
            session_id: "test-session".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind,
        }
    }

    #[test]
    fn materialize_simple_text_turn() {
        let events = vec![
            make_event(
                1,
                100,
                AgentEventKind::PromptReceived {
                    content: "hello".to_string(),
                    message_id: None,
                },
            ),
            make_event(2, 101, AgentEventKind::LlmRequestStart { message_count: 1 }),
            make_event(
                3,
                102,
                AgentEventKind::AssistantMessageStored {
                    content: "hi there".to_string(),
                    thinking: None,
                    message_id: None,
                },
            ),
            make_event(
                4,
                103,
                AgentEventKind::LlmRequestEnd {
                    usage: Some(querymt::Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        ..Default::default()
                    }),
                    tool_calls: 0,
                    finish_reason: Some(querymt::chat::FinishReason::Stop),
                    cost_usd: Some(0.001),
                    cumulative_cost_usd: Some(0.001),
                    context_tokens: 15,
                    metrics: ExecutionMetrics { steps: 1, turns: 1 },
                },
            ),
        ];

        let (turns, _meta) = materialize_turns(&events);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user_content.as_deref(), Some("hello"));
        assert_eq!(turns[0].assistant_content, "hi there");
        assert!(turns[0].tool_calls.is_empty());
        assert!(turns[0].tool_results.is_empty());
        assert_eq!(turns[0].usage.as_ref().unwrap().input_tokens, 10);
        assert_eq!(turns[0].cost_usd, Some(0.001));
    }

    #[test]
    fn materialize_tool_call_turn() {
        let events = vec![
            make_event(
                1,
                100,
                AgentEventKind::PromptReceived {
                    content: "read test.rs".to_string(),
                    message_id: None,
                },
            ),
            make_event(2, 101, AgentEventKind::LlmRequestStart { message_count: 1 }),
            make_event(
                3,
                102,
                AgentEventKind::AssistantMessageStored {
                    content: "".to_string(),
                    thinking: None,
                    message_id: None,
                },
            ),
            make_event(
                4,
                103,
                AgentEventKind::ToolCallStart {
                    tool_call_id: "call-1".to_string(),
                    tool_name: "read_tool".to_string(),
                    arguments: r#"{"path":"test.rs"}"#.to_string(),
                },
            ),
            make_event(
                5,
                104,
                AgentEventKind::ToolCallEnd {
                    tool_call_id: "call-1".to_string(),
                    tool_name: "read_tool".to_string(),
                    is_error: false,
                    result: "fn main() {}".to_string(),
                },
            ),
            make_event(
                6,
                105,
                AgentEventKind::LlmRequestEnd {
                    usage: None,
                    tool_calls: 1,
                    finish_reason: Some(querymt::chat::FinishReason::ToolCalls),
                    cost_usd: None,
                    cumulative_cost_usd: None,
                    context_tokens: 50,
                    metrics: ExecutionMetrics { steps: 1, turns: 1 },
                },
            ),
        ];

        let (turns, _meta) = materialize_turns(&events);
        assert_eq!(turns.len(), 1);
        assert!(turns[0].assistant_content.is_empty());
        assert_eq!(turns[0].tool_calls.len(), 1);
        assert_eq!(turns[0].tool_calls[0].name, "read_tool");
        assert_eq!(turns[0].tool_results.len(), 1);
        assert_eq!(turns[0].tool_results[0].content, "fn main() {}");
        assert!(!turns[0].tool_results[0].is_error);
    }

    #[test]
    fn materialize_extracts_metadata() {
        let events = vec![
            make_event(
                1,
                100,
                AgentEventKind::ProviderChanged {
                    provider: "anthropic".to_string(),
                    model: "claude-opus-4-6".to_string(),
                    config_id: 1,
                    context_limit: Some(200_000),
                    provider_node_id: None,
                },
            ),
            make_event(
                2,
                101,
                AgentEventKind::MiddlewareInjected {
                    message: "You are a helpful assistant.".to_string(),
                },
            ),
            make_event(
                3,
                102,
                AgentEventKind::ToolsAvailable {
                    tools: vec![querymt::chat::Tool {
                        tool_type: "function".to_string(),
                        function: querymt::chat::FunctionTool {
                            name: "shell".to_string(),
                            description: "Run a command".to_string(),
                            parameters: serde_json::json!({}),
                        },
                    }],
                    tools_hash: Default::default(),
                },
            ),
        ];

        let (turns, meta) = materialize_turns(&events);
        assert!(turns.is_empty());
        assert_eq!(meta.initial_model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(meta.initial_provider.as_deref(), Some("anthropic"));
        assert_eq!(meta.system_prompts.len(), 1);
        assert_eq!(meta.system_prompts[0], "You are a helpful assistant.");
        assert!(meta.tool_definitions.is_some());
    }

    #[test]
    fn materialize_multi_turn_tracks_model_changes() {
        let events = vec![
            make_event(
                1,
                100,
                AgentEventKind::ProviderChanged {
                    provider: "anthropic".to_string(),
                    model: "claude-opus-4-6".to_string(),
                    config_id: 1,
                    context_limit: None,
                    provider_node_id: None,
                },
            ),
            make_event(
                2,
                101,
                AgentEventKind::PromptReceived {
                    content: "turn 1".to_string(),
                    message_id: None,
                },
            ),
            make_event(3, 102, AgentEventKind::LlmRequestStart { message_count: 1 }),
            make_event(
                4,
                103,
                AgentEventKind::AssistantMessageStored {
                    content: "response 1".to_string(),
                    thinking: None,
                    message_id: None,
                },
            ),
            make_event(
                5,
                104,
                AgentEventKind::LlmRequestEnd {
                    usage: None,
                    tool_calls: 0,
                    finish_reason: Some(querymt::chat::FinishReason::Stop),
                    cost_usd: None,
                    cumulative_cost_usd: None,
                    context_tokens: 0,
                    metrics: ExecutionMetrics::default(),
                },
            ),
            // Model changes
            make_event(
                6,
                200,
                AgentEventKind::ProviderChanged {
                    provider: "openai".to_string(),
                    model: "gpt-4".to_string(),
                    config_id: 2,
                    context_limit: None,
                    provider_node_id: None,
                },
            ),
            make_event(
                7,
                201,
                AgentEventKind::PromptReceived {
                    content: "turn 2".to_string(),
                    message_id: None,
                },
            ),
            make_event(8, 202, AgentEventKind::LlmRequestStart { message_count: 2 }),
            make_event(
                9,
                203,
                AgentEventKind::AssistantMessageStored {
                    content: "response 2".to_string(),
                    thinking: None,
                    message_id: None,
                },
            ),
            make_event(
                10,
                204,
                AgentEventKind::LlmRequestEnd {
                    usage: None,
                    tool_calls: 0,
                    finish_reason: Some(querymt::chat::FinishReason::Stop),
                    cost_usd: None,
                    cumulative_cost_usd: None,
                    context_tokens: 0,
                    metrics: ExecutionMetrics::default(),
                },
            ),
        ];

        let (turns, meta) = materialize_turns(&events);
        assert_eq!(turns.len(), 2);
        assert_eq!(meta.initial_model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(turns[0].model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(turns[1].model.as_deref(), Some("gpt-4"));
    }

    #[test]
    fn materialize_thinking_content() {
        let events = vec![
            make_event(1, 100, AgentEventKind::LlmRequestStart { message_count: 1 }),
            make_event(
                2,
                101,
                AgentEventKind::AssistantMessageStored {
                    content: "answer".to_string(),
                    thinking: Some("let me think...".to_string()),
                    message_id: None,
                },
            ),
            make_event(
                3,
                102,
                AgentEventKind::LlmRequestEnd {
                    usage: None,
                    tool_calls: 0,
                    finish_reason: Some(querymt::chat::FinishReason::Stop),
                    cost_usd: None,
                    cumulative_cost_usd: None,
                    context_tokens: 0,
                    metrics: ExecutionMetrics::default(),
                },
            ),
        ];

        let (turns, _) = materialize_turns(&events);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].assistant_content, "answer");
        assert_eq!(turns[0].thinking.as_deref(), Some("let me think..."));
        // No user_content for this turn
        assert!(turns[0].user_content.is_none());
    }
}
