use crate::events::StopType;
use crate::middleware::{
    AgentStats, CompositeDriver, ConversationContext, ExecutionState, LimitsConfig,
    LimitsMiddleware, MaxStepsMiddleware, MiddlewareDriver, PriceLimitMiddleware,
    TurnLimitMiddleware,
};
use crate::test_utils::{
    CancelDriver, CompleteDriver, CountingDriver, ErrorDriver, PassThroughDriver, StopDriver,
    test_context, test_context_with_user_messages,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn test_composite_driver_empty_passes_through() {
    let composite = CompositeDriver::new(vec![]);
    let context = test_context("sess-1", 0);
    let state = ExecutionState::BeforeLlmCall { context };

    let result = composite.run_turn_start(state, None).await.unwrap();

    assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));
}

#[tokio::test]
async fn test_composite_driver_single_driver() {
    let drivers: Vec<Arc<dyn MiddlewareDriver>> = vec![Arc::new(PassThroughDriver)];
    let composite = CompositeDriver::new(drivers);
    let context = test_context("sess-1", 0);
    let state = ExecutionState::BeforeLlmCall { context };

    let result = composite.run_turn_start(state, None).await.unwrap();

    assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));
}

#[tokio::test]
async fn test_composite_driver_chain_order() {
    let counter1 = Arc::new(CountingDriver {
        count: AtomicUsize::new(0),
    });
    let counter2 = Arc::new(CountingDriver {
        count: AtomicUsize::new(0),
    });

    let drivers: Vec<Arc<dyn MiddlewareDriver>> = vec![counter1.clone(), counter2.clone()];
    let composite = CompositeDriver::new(drivers);
    let context = test_context("sess-1", 0);
    let state = ExecutionState::BeforeLlmCall { context };

    composite.run_turn_start(state, None).await.unwrap();

    assert_eq!(counter1.count.load(Ordering::SeqCst), 1);
    assert_eq!(counter2.count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_composite_driver_stopped_halts() {
    let counter = Arc::new(CountingDriver {
        count: AtomicUsize::new(0),
    });

    let drivers: Vec<Arc<dyn MiddlewareDriver>> = vec![
        Arc::new(StopDriver {
            stop_type: StopType::Other,
            message: "stopped",
        }),
        counter.clone(),
    ];
    let composite = CompositeDriver::new(drivers);
    let context = test_context("sess-1", 0);
    let state = ExecutionState::BeforeLlmCall { context };

    let result = composite.run_turn_start(state, None).await.unwrap();

    assert!(matches!(result, ExecutionState::Stopped { .. }));
    assert_eq!(counter.count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_composite_driver_complete_halts() {
    let counter = Arc::new(CountingDriver {
        count: AtomicUsize::new(0),
    });

    let drivers: Vec<Arc<dyn MiddlewareDriver>> = vec![Arc::new(CompleteDriver), counter.clone()];
    let composite = CompositeDriver::new(drivers);
    let context = test_context("sess-1", 0);
    let state = ExecutionState::BeforeLlmCall { context };

    let result = composite.run_turn_start(state, None).await.unwrap();

    assert!(matches!(result, ExecutionState::Complete));
    assert_eq!(counter.count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_composite_driver_cancelled_halts() {
    let counter = Arc::new(CountingDriver {
        count: AtomicUsize::new(0),
    });

    let drivers: Vec<Arc<dyn MiddlewareDriver>> = vec![Arc::new(CancelDriver), counter.clone()];
    let composite = CompositeDriver::new(drivers);
    let context = test_context("sess-1", 0);
    let state = ExecutionState::BeforeLlmCall { context };

    let result = composite.run_turn_start(state, None).await.unwrap();

    assert!(matches!(result, ExecutionState::Cancelled));
    assert_eq!(counter.count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_composite_driver_reset_all() {
    let counter1 = Arc::new(CountingDriver {
        count: AtomicUsize::new(5),
    });
    let counter2 = Arc::new(CountingDriver {
        count: AtomicUsize::new(10),
    });

    let drivers: Vec<Arc<dyn MiddlewareDriver>> = vec![counter1.clone(), counter2.clone()];
    let composite = CompositeDriver::new(drivers);

    composite.reset();

    assert_eq!(counter1.count.load(Ordering::SeqCst), 0);
    assert_eq!(counter2.count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_composite_driver_error_propagates() {
    let drivers: Vec<Arc<dyn MiddlewareDriver>> =
        vec![Arc::new(PassThroughDriver), Arc::new(ErrorDriver)];
    let composite = CompositeDriver::new(drivers);
    let context = test_context("sess-1", 0);
    let state = ExecutionState::BeforeLlmCall { context };

    let result = composite.run_turn_start(state, None).await;

    assert!(result.is_err());
}

#[test]
fn test_composite_driver_len_and_is_empty() {
    let empty = CompositeDriver::new(vec![]);
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);

    let drivers: Vec<Arc<dyn MiddlewareDriver>> =
        vec![Arc::new(PassThroughDriver), Arc::new(PassThroughDriver)];
    let composite = CompositeDriver::new(drivers);
    assert!(!composite.is_empty());
    assert_eq!(composite.len(), 2);
}

#[tokio::test]
async fn test_max_steps_at_limit_stops() {
    let middleware = MaxStepsMiddleware::new(5);
    let context = test_context("sess-1", 5);
    let state = ExecutionState::BeforeLlmCall { context };

    let result = middleware.on_step_start(state, None).await.unwrap();

    assert!(matches!(
        result,
        ExecutionState::Stopped {
            stop_type: StopType::StepLimit,
            ..
        }
    ));
}

#[tokio::test]
async fn test_max_steps_below_limit_continues() {
    let middleware = MaxStepsMiddleware::new(5);
    let context = test_context("sess-1", 3);
    let state = ExecutionState::BeforeLlmCall { context };

    let result = middleware.on_step_start(state, None).await.unwrap();

    assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));
}

#[tokio::test]
async fn test_max_steps_ignores_other_states() {
    let middleware = MaxStepsMiddleware::new(5);
    let context = test_context("sess-1", 10);

    let state = ExecutionState::CallLlm {
        context: context.clone(),
        tools: Arc::from([]),
    };
    let result = middleware.on_step_start(state, None).await.unwrap();
    assert!(matches!(result, ExecutionState::CallLlm { .. }));

    let state = ExecutionState::Complete;
    let result = middleware.on_step_start(state, None).await.unwrap();
    assert!(matches!(result, ExecutionState::Complete));
}

#[tokio::test]
async fn test_turn_limit_middleware() {
    let middleware = TurnLimitMiddleware::new(3);

    // With 2 user messages, should continue (2 < 3)
    let context = test_context_with_user_messages("sess-1", 2);
    let state = ExecutionState::BeforeLlmCall { context };
    let result = middleware.on_step_start(state, None).await.unwrap();
    assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));

    // With 3 user messages, should stop (3 >= 3)
    let context = test_context_with_user_messages("sess-1", 3);
    let state = ExecutionState::BeforeLlmCall { context };
    let result = middleware.on_step_start(state, None).await.unwrap();
    assert!(matches!(result, ExecutionState::Stopped { .. }));
}

#[tokio::test]
async fn test_price_limit_over_budget() {
    let middleware = PriceLimitMiddleware::new(0.10, 3.0, 15.0);
    let stats = Arc::new(AgentStats {
        steps: 1,
        total_input_tokens: 100_000,
        total_output_tokens: 10_000,
        context_tokens: 0,
        ..Default::default()
    });
    let context = Arc::new(ConversationContext::new(
        "sess-1".into(),
        Arc::from([]),
        stats,
        "mock".into(),
        "mock-model".into(),
    ));
    let state = ExecutionState::BeforeLlmCall { context };

    let result = middleware.on_step_start(state, None).await.unwrap();

    assert!(matches!(
        result,
        ExecutionState::Stopped {
            stop_type: StopType::PriceLimit,
            ..
        }
    ));
}

#[tokio::test]
async fn test_price_limit_under_budget() {
    let middleware = PriceLimitMiddleware::new(1.0, 3.0, 15.0);
    let stats = Arc::new(AgentStats {
        steps: 1,
        total_input_tokens: 10_000,
        total_output_tokens: 1_000,
        context_tokens: 0,
        ..Default::default()
    });
    let context = Arc::new(ConversationContext::new(
        "sess-1".into(),
        Arc::from([]),
        stats,
        "mock".into(),
        "mock-model".into(),
    ));
    let state = ExecutionState::BeforeLlmCall { context };

    let result = middleware.on_step_start(state, None).await.unwrap();

    assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));
}

#[tokio::test]
async fn test_price_limit_missing_config_passes_through() {
    let config = LimitsConfig::default().max_price_usd(0.10);
    let middleware = LimitsMiddleware::new(config);

    let stats = Arc::new(AgentStats {
        steps: 1,
        total_input_tokens: 1_000_000,
        total_output_tokens: 1_000_000,
        context_tokens: 0,
        ..Default::default()
    });
    let context = Arc::new(ConversationContext::new(
        "sess-1".into(),
        Arc::from([]),
        stats,
        "mock".into(),
        "mock-model".into(),
    ));
    let state = ExecutionState::BeforeLlmCall { context };

    let result = middleware.on_step_start(state, None).await.unwrap();

    assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));
}

#[tokio::test]
async fn test_limits_middleware_combined() {
    let config = LimitsConfig::with_manual_pricing(3.0, 15.0)
        .max_steps(10)
        .max_turns(5)
        .max_price_usd(1.0);
    let middleware = LimitsMiddleware::new(config);

    let context = test_context("sess-1", 2);
    let state = ExecutionState::BeforeLlmCall { context };
    let result = middleware.on_step_start(state, None).await.unwrap();
    assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));

    let context = test_context("sess-1", 10);
    let state = ExecutionState::BeforeLlmCall { context };
    let result = middleware.on_step_start(state, None).await.unwrap();
    assert!(matches!(result, ExecutionState::Stopped { .. }));
}

// test_context moved to crate::test_utils::helpers
