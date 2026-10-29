use crate::events::StopType;
use crate::middleware::{
    AgentStats, ConversationContext, ExecutionState, LlmResponse, PreparedModelRequest, ToolResult,
    WaitCondition, WaitReason,
};
use crate::test_utils::{mock_tool_call, test_context};
use querymt::chat::{ChatMessage, ChatRole, FinishReason, ToolResultPart};
use std::sync::Arc;

#[test]
fn test_execution_state_name_all_variants() {
    let context = test_context("sess-1", 0);
    let response = Arc::new(LlmResponse::new(
        "".to_string(),
        vec![],
        None,
        Some(FinishReason::Stop),
    ));
    let states = vec![
        (
            ExecutionState::BeforeLlmCall {
                context: context.clone(),
            },
            "BeforeLlmCall",
        ),
        (
            ExecutionState::CallLlm {
                context: context.clone(),
                request: Arc::new(PreparedModelRequest {
                    messages: Arc::from([]),
                    tools: Arc::from([]),
                    estimated_tokens: 0,
                    options: Default::default(),
                }),
            },
            "CallLlm",
        ),
        (
            ExecutionState::AfterLlm {
                response: response.clone(),
                context: context.clone(),
            },
            "AfterLlm",
        ),
        (
            ExecutionState::ProcessingToolCalls {
                remaining_calls: Arc::from([]),
                results: Arc::from([]),
                context: context.clone(),
            },
            "ProcessingToolCalls",
        ),
        (
            ExecutionState::WaitingForEvent {
                context: context.clone(),
                wait: WaitCondition::delegation("del-1".to_string()),
            },
            "WaitingForEvent",
        ),
        (
            ExecutionState::Complete {
                context: context.clone(),
            },
            "Complete",
        ),
        (
            ExecutionState::Stopped {
                message: "done".into(),
                stop_type: StopType::Other,
                context: None,
            },
            "Stopped",
        ),
        (ExecutionState::Cancelled, "Cancelled"),
    ];

    for (state, expected) in states {
        assert_eq!(state.name(), expected);
    }
}

#[test]
fn test_execution_state_context_accessors() {
    let context = test_context("sess-1", 0);
    let response = Arc::new(LlmResponse::new(
        "".to_string(),
        vec![],
        None,
        Some(FinishReason::Stop),
    ));

    let stateful_states = vec![
        ExecutionState::BeforeLlmCall {
            context: context.clone(),
        },
        ExecutionState::CallLlm {
            context: context.clone(),
            request: Arc::new(PreparedModelRequest {
                messages: Arc::from([]),
                tools: Arc::from([]),
                estimated_tokens: 0,
                options: Default::default(),
            }),
        },
        ExecutionState::AfterLlm {
            response: response.clone(),
            context: context.clone(),
        },
        ExecutionState::ProcessingToolCalls {
            remaining_calls: Arc::from([]),
            results: Arc::from([]),
            context: context.clone(),
        },
        ExecutionState::WaitingForEvent {
            context: context.clone(),
            wait: WaitCondition::delegation("del-1".to_string()),
        },
        ExecutionState::Complete {
            context: context.clone(),
        },
    ];

    for state in stateful_states {
        assert!(state.context().is_some());
    }

    let terminal_states = vec![
        ExecutionState::Stopped {
            message: "done".into(),
            stop_type: StopType::Other,
            context: None,
        },
        ExecutionState::Cancelled,
    ];

    for state in terminal_states {
        assert!(state.context().is_none());
    }
}

#[test]
fn test_conversation_context_counts_user_messages() {
    let messages = vec![
        ChatMessage::user().text("one").build(),
        ChatMessage::assistant().text("two").build(),
        ChatMessage::user().text("three").build(),
    ];
    let stats = AgentStats {
        turns: 2,
        ..Default::default()
    };
    let context = ConversationContext::new(
        "sess-1".into(),
        Arc::from(messages.into_boxed_slice()),
        Arc::new(stats),
        "mock".into(),
        "mock-model".into(),
    );

    assert_eq!(context.user_message_count(), 2);
    assert!(!context.is_first_turn());
}

#[test]
fn test_conversation_context_inject_message() {
    let context = ConversationContext::new(
        "sess-1".into(),
        Arc::from([]),
        Arc::new(AgentStats::default()),
        "mock".into(),
        "mock-model".into(),
    );

    let injected = context.inject_message("hello".to_string());

    assert_eq!(context.messages.len(), 0);
    assert_eq!(injected.messages.len(), 1);
    assert!(matches!(injected.messages[0].role, ChatRole::User));
    assert_eq!(injected.messages[0].text(), "hello");
    assert!(injected.is_first_turn());
}

#[test]
fn test_llm_response_has_tool_calls() {
    let no_tools = LlmResponse::new("ok".to_string(), vec![], None, Some(FinishReason::Stop));
    assert!(!no_tools.has_tool_calls());

    let with_tools = LlmResponse::new(
        "ok".to_string(),
        vec![mock_tool_call("call-1", "tool", "{}")],
        None,
        Some(FinishReason::ToolCalls),
    );
    assert!(with_tools.has_tool_calls());
}

#[test]
fn test_tool_result_with_snapshot() {
    let result = ToolResult::new(
        "call-1".to_string(),
        vec![ToolResultPart::text("ok")],
        false,
        Some("tool".to_string()),
        Some("{}".to_string()),
    );

    assert!(result.snapshot_part.is_none());

    let updated = result.with_snapshot(crate::model::MessagePart::Snapshot {
        root_hash: crate::hash::RapidHash::new(b"hash"),
        changed_paths: crate::index::merkle::DiffPaths::default(),
    });

    assert!(updated.snapshot_part.is_some());
}

#[test]
fn test_wait_condition_merge() {
    let conditions = vec![
        WaitCondition::delegation("del-1".to_string()),
        WaitCondition::delegation("del-2".to_string()),
    ];
    let merged = WaitCondition::merge(conditions).unwrap();

    assert_eq!(merged.reason, WaitReason::Delegation);
    assert_eq!(merged.correlation_ids.len(), 2);
    assert!(merged.correlation_ids.contains(&"del-1".to_string()));
    assert!(merged.correlation_ids.contains(&"del-2".to_string()));

    assert!(WaitCondition::merge(vec![]).is_none());
}

#[test]
fn test_agent_stats_default() {
    let stats = AgentStats::default();
    assert_eq!(stats.steps, 0);
    assert_eq!(stats.turns, 0);
    assert_eq!(stats.total_input_tokens, 0);
    assert_eq!(stats.total_output_tokens, 0);
    assert_eq!(stats.context_tokens, 0);
}

// Helper functions moved to test_utils module

#[test]
fn recorded_costs_view_reports_components_without_repricing() {
    use crate::accounting::{ComponentSums, ScopeTotals};
    // Token counters at one rate must not influence the monetary view.
    let mut stats = AgentStats {
        total_input_tokens: 1_000_000,
        total_output_tokens: 1_000_000,
        ..Default::default()
    };
    let totals: ScopeTotals = serde_json::from_value(serde_json::json!({
        "known_subtotal_usd": 0.0057652,
        "budget_known_subtotal_usd": 0.0057652,
        "components": ComponentSums {
            input_usd: 0.00014,
            output_usd: 0.0056,
            cache_read_usd: 0.0000252,
            cache_write_usd: 0.0,
            cache_write_1h_usd: 0.0,
        },
        "priced_attempts": 1, "excluded_attempts": 0, "unresolved_attempts": 0,
        "incomplete_attempts": 0, "budget_incomplete_attempts": 0,
        "settled_reasons": [],
    }))
    .unwrap();
    stats.apply_recorded_costs(&totals);
    assert!((stats.input_cost_usd - 0.00014).abs() < 1e-12);
    assert!((stats.output_cost_usd - 0.0056).abs() < 1e-12);
    assert!((stats.cache_read_cost_usd - 0.0000252).abs() < 1e-12);
    assert!((stats.total_cost_usd - 0.0057652).abs() < 1e-12);

    // Applying an empty scope clears the view (a new cycle starts at zero).
    stats.apply_recorded_costs(&ScopeTotals::default());
    assert_eq!(stats.total_cost_usd, 0.0);
    assert_eq!(stats.output_cost_usd, 0.0);
}
