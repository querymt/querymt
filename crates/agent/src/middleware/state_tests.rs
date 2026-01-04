use crate::middleware::{
    AgentStats, ConversationContext, ExecutionState, LlmResponse, ToolResult, WaitCondition,
    WaitReason,
};
use crate::test_utils::{mock_tool_call, test_context};
use agent_client_protocol::StopReason;
use querymt::chat::{ChatMessage, ChatRole, FinishReason, MessageType};
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
    let tool_call = Arc::new(mock_tool_call("call-1", "tool", "{}"));
    let tool_result = Arc::new(ToolResult::new(
        "call-1".to_string(),
        "ok".to_string(),
        false,
        None,
        None,
    ));
    let states = vec![
        (
            ExecutionState::BeforeTurn {
                context: context.clone(),
            },
            "BeforeTurn",
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
            ExecutionState::BeforeToolCall {
                call: tool_call.clone(),
                context: context.clone(),
            },
            "BeforeToolCall",
        ),
        (
            ExecutionState::AfterTool {
                result: tool_result.clone(),
                context: context.clone(),
            },
            "AfterTool",
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
                reason: StopReason::EndTurn,
                message: "done".into(),
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
    let tool_call = Arc::new(mock_tool_call("call-1", "tool", "{}"));
    let tool_result = Arc::new(ToolResult::new(
        "call-1".to_string(),
        "ok".to_string(),
        false,
        None,
        None,
    ));

    let stateful_states = vec![
        ExecutionState::BeforeTurn {
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
        ExecutionState::BeforeToolCall {
            call: tool_call.clone(),
            context: context.clone(),
        },
        ExecutionState::AfterTool {
            result: tool_result.clone(),
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
            reason: StopReason::EndTurn,
            message: "done".into(),
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
            content: "one".to_string(),
            message_type: MessageType::Text,
        },
        ChatMessage {
            role: ChatRole::Assistant,
            content: "two".to_string(),
            message_type: MessageType::Text,
        },
        ChatMessage {
            role: ChatRole::User,
            content: "three".to_string(),
            message_type: MessageType::Text,
        },
    ];
    let context = ConversationContext::new(
        "sess-1".into(),
        Arc::from(messages.into_boxed_slice()),
        Arc::new(AgentStats::default()),
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
    assert_eq!(injected.messages[0].content, "hello");
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
        "ok".to_string(),
        false,
        Some("tool".to_string()),
        Some("{}".to_string()),
    );

    assert!(result.snapshot_part.is_none());

    let updated = result.with_snapshot(crate::model::MessagePart::Snapshot {
        root_hash: crate::hash::RapidHash::new(b"hash"),
        diff_summary: Some("summary".to_string()),
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
    assert_eq!(stats.total_input_tokens, 0);
    assert_eq!(stats.total_output_tokens, 0);
    assert_eq!(stats.context_tokens, 0);
}

// Helper functions moved to test_utils module
