use crate::events::StopType;
use crate::middleware::{
    AgentStats, CompositeDriver, ConversationContext, ExecutionState, LlmResponse, MiddlewareDriver,
};
use crate::test_utils::{
    AlwaysStopDriver, BeforeLlmCallToCallLlmDriver, MessageInjectingDriver, StateRecordingDriver,
    test_context,
};
use querymt::chat::FinishReason;
use std::sync::Arc;

#[tokio::test]
async fn test_state_recording_middleware() {
    let (recorder, states) = StateRecordingDriver::new();
    let composite = CompositeDriver::new(vec![Arc::new(recorder)]);
    let context = test_context("sess-1", 0);
    let response = Arc::new(LlmResponse::new(
        "".to_string(),
        vec![],
        None,
        Some(FinishReason::Stop),
    ));

    composite
        .run_turn_start(ExecutionState::BeforeLlmCall {
            context: context.clone(),
        })
        .await
        .unwrap();
    composite
        .run_step_start(ExecutionState::BeforeLlmCall {
            context: context.clone(),
        })
        .await
        .unwrap();
    composite
        .run_after_llm(ExecutionState::AfterLlm {
            response,
            context: context.clone(),
        })
        .await
        .unwrap();

    let recorded = states.lock().unwrap();
    assert_eq!(recorded.len(), 3);
    assert_eq!(recorded[0], "BeforeLlmCall");
    assert_eq!(recorded[1], "BeforeLlmCall");
    assert_eq!(recorded[2], "AfterLlm");
}

#[tokio::test]
async fn test_message_injection_flow() {
    let injector = MessageInjectingDriver {
        inject_content: "Remember to be helpful".to_string(),
    };
    let composite = CompositeDriver::new(vec![Arc::new(injector)]);
    let context = test_context("sess-1", 0);
    let state = ExecutionState::BeforeLlmCall { context };

    let result = composite.run_turn_start(state).await.unwrap();

    if let ExecutionState::BeforeLlmCall { context } = result {
        assert_eq!(context.messages.len(), 1);
        assert!(
            context.messages[0]
                .content
                .contains("Remember to be helpful")
        );
    } else {
        panic!("Expected BeforeLlmCall state");
    }
}

#[tokio::test]
async fn test_multiple_middleware_interaction() {
    let (recorder, states) = StateRecordingDriver::new();
    let injector = MessageInjectingDriver {
        inject_content: "injected".to_string(),
    };
    let composite = CompositeDriver::new(vec![Arc::new(recorder), Arc::new(injector)]);
    let context = test_context("sess-1", 0);

    let result = composite
        .run_turn_start(ExecutionState::BeforeLlmCall { context })
        .await
        .unwrap();

    assert_eq!(states.lock().unwrap().len(), 1);

    if let ExecutionState::BeforeLlmCall { context } = result {
        assert_eq!(context.messages.len(), 1);
    } else {
        panic!("Expected BeforeLlmCall state");
    }
}

#[tokio::test]
async fn test_middleware_can_transform_call_llm() {
    let composite = CompositeDriver::new(vec![Arc::new(BeforeLlmCallToCallLlmDriver)]);
    let context = test_context("sess-1", 0);

    let result = composite
        .run_step_start(ExecutionState::BeforeLlmCall { context })
        .await
        .unwrap();

    assert!(matches!(result, ExecutionState::CallLlm { .. }));
}

#[tokio::test]
async fn test_middleware_can_transform_to_stopped() {
    let composite = CompositeDriver::new(vec![Arc::new(AlwaysStopDriver {
        stop_type: StopType::Other,
    })]);
    let context = test_context("sess-1", 0);
    let response = Arc::new(LlmResponse::new(
        "".to_string(),
        vec![],
        None,
        Some(FinishReason::Stop),
    ));

    let result = composite
        .run_turn_start(ExecutionState::BeforeLlmCall {
            context: context.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(result, ExecutionState::Stopped { .. }));

    let result = composite
        .run_step_start(ExecutionState::BeforeLlmCall {
            context: context.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(result, ExecutionState::Stopped { .. }));

    let result = composite
        .run_after_llm(ExecutionState::AfterLlm {
            response,
            context: context.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(result, ExecutionState::Stopped { .. }));
}

#[tokio::test]
async fn test_stats_propagate_through_middleware() {
    let (recorder, _) = StateRecordingDriver::new();
    let composite = CompositeDriver::new(vec![Arc::new(recorder)]);
    let stats = Arc::new(AgentStats {
        steps: 42,
        total_input_tokens: 1000,
        total_output_tokens: 500,
        context_tokens: 1500,
        ..Default::default()
    });
    let context = Arc::new(ConversationContext::new(
        "sess-1".into(),
        Arc::from([]),
        stats,
        "mock".into(),
        "mock-model".into(),
    ));

    let result = composite
        .run_turn_start(ExecutionState::BeforeLlmCall { context })
        .await
        .unwrap();

    if let ExecutionState::BeforeLlmCall { context } = result {
        assert_eq!(context.stats.steps, 42);
        assert_eq!(context.stats.total_input_tokens, 1000);
    } else {
        panic!("Expected BeforeLlmCall state");
    }
}

#[tokio::test]
async fn test_context_immutability() {
    let injector = MessageInjectingDriver {
        inject_content: "new message".to_string(),
    };
    let composite = CompositeDriver::new(vec![Arc::new(injector)]);
    let original = test_context("sess-1", 0);
    let original_len = original.messages.len();

    let result = composite
        .run_turn_start(ExecutionState::BeforeLlmCall {
            context: original.clone(),
        })
        .await
        .unwrap();

    assert_eq!(original.messages.len(), original_len);

    if let ExecutionState::BeforeLlmCall { context } = result {
        assert_eq!(context.messages.len(), 1);
    } else {
        panic!("Expected BeforeLlmCall state");
    }
}

#[tokio::test]
async fn test_driver_name_returns_correct_value() {
    let composite = CompositeDriver::new(vec![]);
    assert_eq!(composite.name(), "CompositeDriver");

    let driver = AlwaysStopDriver {
        stop_type: StopType::Other,
    };
    assert_eq!(driver.name(), "AlwaysStopDriver");
}
