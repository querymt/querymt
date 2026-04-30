use crate::events::StopType;
use crate::middleware::{
    AgentStats, ConversationContext, ExecutionState, LlmResponse, ToolResult, WaitCondition,
    WaitReason,
};
use crate::test_utils::{mock_tool_call, test_context};
use querymt::chat::{ChatMessage, ChatRole, Content, FinishReason};
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
                tools: Arc::from([]),
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
        (ExecutionState::Complete, "Complete"),
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
            tools: Arc::from([]),
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
    ];

    for state in stateful_states {
        assert!(state.context().is_some());
    }

    let terminal_states = vec![
        ExecutionState::Complete,
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
        ChatMessage {
            role: ChatRole::User,
            content: vec![Content::text("one")],
            cache: None,
        },
        ChatMessage {
            role: ChatRole::Assistant,
            content: vec![Content::text("two")],
            cache: None,
        },
        ChatMessage {
            role: ChatRole::User,
            content: vec![Content::text("three")],
            cache: None,
        },
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
        vec![Content::text("ok")],
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
fn test_update_costs_includes_reasoning_tokens_in_output_cost() {
    // Reasoning tokens must be billed at the output rate (no separate reasoning pricing).
    // DeepSeek V4-Flash pricing: input $0.14/M, output $0.28/M, cache_read $0.0028/M
    let pricing = querymt::providers::ModelPricing {
        input: Some(0.14),
        output: Some(0.28),
        cache_read: Some(0.0028),
        cache_write: None,
    };

    let mut stats = AgentStats {
        total_input_tokens: 1_000, // cache miss tokens (after subtracting cache_read)
        total_output_tokens: 2_000, // non-reasoning output tokens
        reasoning_tokens: 18_000,  // reasoning tokens (should be billed at output rate)
        cache_read_tokens: 9_000,
        ..Default::default()
    };

    stats.update_costs(&pricing);

    // billable_output = 2_000 + 18_000 = 20_000
    // input_cost  = 1_000   × $0.14/M  = $0.000140
    // output_cost = 20_000  × $0.28/M  = $0.005600
    // cache_read  = 9_000   × $0.0028/M = $0.0000252
    // total = $0.005600 + $0.000140 + $0.0000252 = $0.0057652
    assert!((stats.input_cost_usd - 0.00014).abs() < 1e-10);
    assert!((stats.output_cost_usd - 0.0056).abs() < 1e-10);
    assert!((stats.cache_read_cost_usd - 0.0000252).abs() < 1e-10);
    assert!((stats.total_cost_usd - 0.0057652).abs() < 1e-6);
}

#[test]
fn test_update_costs_without_reasoning_tokens_unchanged() {
    // When reasoning_tokens = 0, cost should be same as before
    let pricing = querymt::providers::ModelPricing {
        input: Some(3.0),
        output: Some(15.0),
        cache_read: None,
        cache_write: None,
    };

    let mut stats = AgentStats {
        total_input_tokens: 1_000_000,
        total_output_tokens: 100_000,
        reasoning_tokens: 0,
        ..Default::default()
    };

    stats.update_costs(&pricing);

    // input:  1_000_000 × $3/M  = $3.00
    // output: 100_000 × $15/M   = $1.50
    // total = $4.50
    assert!((stats.total_cost_usd - 4.50).abs() < 1e-6);
    assert!((stats.input_cost_usd - 3.0).abs() < 1e-6);
    assert!((stats.output_cost_usd - 1.50).abs() < 1e-6);
}
