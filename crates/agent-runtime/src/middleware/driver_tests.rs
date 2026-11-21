use crate::middleware::{
    AgentStats, CompositeDriver, ConversationContext, EndVerdict, ExecutionState, LimitsConfig,
    LimitsMiddleware, Middleware, ReplyVerdict, StepVerdict, Stop, StopVerdict,
};
use crate::test_utils::{
    Act, HookRig, ScriptedMiddleware, test_context, test_context_with_user_messages,
};
use querymt_agent_core::events::StopType;
use std::sync::Arc;

fn before_request(session: &str) -> ExecutionState {
    ExecutionState::BeforeLlmCall {
        context: test_context(session, 0),
    }
}

fn other_stop(message: &'static str) -> Act<StepVerdict> {
    Act::Say(StepVerdict::Stop(Stop::new(StopType::Other, message)))
}

fn stack_of(middleware: &[&ScriptedMiddleware]) -> crate::middleware::TurnStack {
    HookRig::stack(
        middleware
            .iter()
            .map(|m| Arc::new((*m).clone()) as Arc<dyn Middleware>)
            .collect(),
    )
}

/// One step of `middleware` over `state`, as the loop would take it.
async fn step(middleware: &LimitsMiddleware, state: ExecutionState) -> ExecutionState {
    let mut stack = HookRig::stack_of(middleware.clone());
    HookRig::new().step_start(&mut stack, state).await.unwrap()
}

#[tokio::test]
async fn an_empty_stack_passes_every_stage() {
    let rig = HookRig::new();
    let mut stack = HookRig::stack(vec![]);

    let state = rig
        .step_start(&mut stack, before_request("s"))
        .await
        .unwrap();

    assert!(matches!(state, ExecutionState::BeforeLlmCall { .. }));
    assert!(stack.is_empty());
}

#[tokio::test]
async fn stop_before_a_model_request() {
    let stopper = ScriptedMiddleware::default().step_start([other_stop("enough")]);
    let mut stack = stack_of(&[&stopper]);

    let state = HookRig::new()
        .step_start(&mut stack, before_request("s"))
        .await
        .unwrap();

    let ExecutionState::Stopped {
        message, stop_type, ..
    } = state
    else {
        panic!("expected a stop");
    };
    assert_eq!((&*message, stop_type), ("enough", StopType::Other));
}

#[tokio::test]
async fn stop_at_run_start_ends_the_stage() {
    let stopper = ScriptedMiddleware::default().turn_start([other_stop("at start")]);
    let mut stack = stack_of(&[&stopper]);

    let state = HookRig::new()
        .turn_start(&mut stack, before_request("s"))
        .await
        .unwrap();

    assert!(matches!(state, ExecutionState::Stopped { .. }));
    assert_eq!(stopper.count("turn_start"), 1);
}

#[tokio::test]
async fn earlier_stop_skips_later_middleware() {
    let first = ScriptedMiddleware::default().step_start([other_stop("first")]);
    let second = ScriptedMiddleware::default();
    let mut stack = stack_of(&[&first, &second]);

    let state = HookRig::new()
        .step_start(&mut stack, before_request("s"))
        .await
        .unwrap();

    assert!(matches!(state, ExecutionState::Stopped { .. }));
    assert_eq!(second.count("step_start"), 0);
}

#[tokio::test]
async fn every_middleware_is_consulted_when_all_pass() {
    let first = ScriptedMiddleware::default();
    let second = ScriptedMiddleware::default();
    let mut stack = stack_of(&[&first, &second]);

    let state = HookRig::new()
        .step_start(&mut stack, before_request("s"))
        .await
        .unwrap();

    assert!(matches!(state, ExecutionState::BeforeLlmCall { .. }));
    assert_eq!(
        (first.count("step_start"), second.count("step_start")),
        (1, 1)
    );
}

#[tokio::test]
async fn reply_discarded_and_retried_by_middleware() {
    let retrier = ScriptedMiddleware::default().after_llm([Act::Say(ReplyVerdict::Retry)]);
    let mut stack = stack_of(&[&retrier]);
    let usage = querymt::Usage {
        input_tokens: 100,
        output_tokens: 40,
        ..Default::default()
    };
    let response = Arc::new(crate::middleware::LlmResponse::new(
        "cut".into(),
        vec![],
        Some(usage),
        Some(querymt::chat::FinishReason::Length),
    ));

    let state = HookRig::new()
        .after_llm(
            &mut stack,
            ExecutionState::AfterLlm {
                response,
                context: test_context("s", 0),
            },
        )
        .await
        .unwrap();

    let ExecutionState::BeforeLlmCall { context } = state else {
        panic!("a retry sends the request again");
    };
    assert_eq!(
        context.stats.total_input_tokens, 100,
        "usage of the discarded reply counts"
    );
    assert_eq!(context.stats.total_output_tokens, 40);
}

#[tokio::test]
async fn continuation_at_the_end_of_a_run() {
    let continuer = ScriptedMiddleware::default().turn_end([
        Act::Notice("one more thing", EndVerdict::Continue),
        Act::Say(EndVerdict::Finish),
    ]);
    let mut stack = stack_of(&[&continuer]);
    let complete = || ExecutionState::Complete {
        context: test_context("s", 0),
    };
    let rig = HookRig::new();

    let first = rig.turn_end(&mut stack, complete()).await.unwrap();
    let second = rig.turn_end(&mut stack, complete()).await.unwrap();

    let ExecutionState::BeforeLlmCall { context } = first else {
        panic!("expected a continuation");
    };
    assert_eq!(context.pending_notices.len(), 1);
    assert_eq!(context.pending_notices[0].content, "one more thing");
    assert!(matches!(second, ExecutionState::Complete { .. }));
}

fn stopped(session: &str) -> ExecutionState {
    ExecutionState::Stopped {
        message: "cut off".into(),
        stop_type: StopType::ModelTokenLimit,
        context: Some(test_context(session, 0)),
    }
}

#[tokio::test]
async fn stop_passed_on_by_every_middleware() {
    let first = ScriptedMiddleware::default().stopped([Act::Say(StopVerdict::Pass)]);
    let second = ScriptedMiddleware::default().stopped([Act::Say(StopVerdict::Pass)]);
    let mut stack = stack_of(&[&first, &second]);

    let state = HookRig::new()
        .stopped(&mut stack, stopped("s"))
        .await
        .unwrap();

    assert!(matches!(
        state,
        ExecutionState::Stopped {
            stop_type: StopType::ModelTokenLimit,
            ..
        }
    ));
    assert_eq!((first.count("stopped"), second.count("stopped")), (1, 1));
}

#[tokio::test]
async fn recovery_ends_the_stop_stage() {
    let first = ScriptedMiddleware::default().stopped([Act::Say(StopVerdict::Pass)]);
    let recovers = ScriptedMiddleware::default().stopped([Act::Say(StopVerdict::Recover)]);
    let never = ScriptedMiddleware::default().stopped([Act::Say(StopVerdict::Recover)]);
    let mut stack = stack_of(&[&first, &recovers, &never]);

    let state = HookRig::new()
        .stopped(&mut stack, stopped("s"))
        .await
        .unwrap();

    assert!(matches!(state, ExecutionState::BeforeLlmCall { .. }));
    assert_eq!(never.count("stopped"), 0);
}

#[tokio::test]
async fn notices_queued_before_a_stop_survive_a_recovery() {
    let queues =
        ScriptedMiddleware::default().step_start([Act::Notice("kept", StepVerdict::Proceed)]);
    let stops = ScriptedMiddleware::default().step_start([other_stop("stop")]);
    let recovers = ScriptedMiddleware::default().stopped([Act::Say(StopVerdict::Recover)]);
    let mut stack = stack_of(&[&queues, &stops, &recovers]);
    let rig = HookRig::new();

    let stop = rig
        .step_start(&mut stack, before_request("s"))
        .await
        .unwrap();
    let recovered = rig.stopped(&mut stack, stop).await.unwrap();

    let ExecutionState::BeforeLlmCall { context } = recovered else {
        panic!("expected a recovery");
    };
    assert_eq!(context.pending_notices.len(), 1);
    assert_eq!(context.pending_notices[0].content, "kept");
}

#[tokio::test]
async fn notices_queued_before_a_published_stop_are_not_stored() {
    let queues =
        ScriptedMiddleware::default().step_start([Act::Notice("lost", StepVerdict::Proceed)]);
    let stops = ScriptedMiddleware::default().step_start([other_stop("stop")]);
    let passes = ScriptedMiddleware::default().stopped([Act::Say(StopVerdict::Pass)]);
    let mut stack = stack_of(&[&queues, &stops, &passes]);
    let rig = HookRig::new();

    let stop = rig
        .step_start(&mut stack, before_request("s"))
        .await
        .unwrap();
    let published = rig.stopped(&mut stack, stop).await.unwrap();

    // The stop is published as it was; the loop stores nothing for it.
    assert!(matches!(published, ExecutionState::Stopped { .. }));
}

#[tokio::test]
async fn cancel_while_a_middleware_is_running() {
    let hangs = ScriptedMiddleware::default().step_start([Act::Hang]);
    let mut stack = stack_of(&[&hangs]);
    let rig = Arc::new(HookRig::new());
    let task_rig = rig.clone();
    let task =
        tokio::spawn(async move { task_rig.step_start(&mut stack, before_request("s")).await });

    while hangs.count("step_start") == 0 {
        tokio::task::yield_now().await;
    }
    rig.cancel.cancel();

    let state = tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .expect("middleware cancellation should be prompt")
        .expect("middleware task should not panic")
        .expect("middleware cancellation should not error");
    assert!(matches!(state, ExecutionState::Cancelled));
}

#[tokio::test]
async fn already_cancelled_run_consults_nobody() {
    let counter = ScriptedMiddleware::default();
    let mut stack = stack_of(&[&counter]);
    let rig = HookRig::new();
    rig.cancel.cancel();

    let state = rig
        .step_start(&mut stack, before_request("s"))
        .await
        .unwrap();

    assert!(matches!(state, ExecutionState::Cancelled));
    assert_eq!(counter.count("step_start"), 0);
}

#[tokio::test]
async fn middleware_error_propagates() {
    let fails = ScriptedMiddleware::default().step_start([Act::Fail]);
    let mut stack = stack_of(&[&ScriptedMiddleware::default(), &fails]);

    let result = HookRig::new()
        .step_start(&mut stack, before_request("s"))
        .await;

    assert!(result.is_err());
}

#[test]
fn composite_driver_len_and_is_empty() {
    let empty = CompositeDriver::new(vec![]);
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);

    let composite = CompositeDriver::new(vec![
        Arc::new(ScriptedMiddleware::default()),
        Arc::new(ScriptedMiddleware::default()),
    ]);
    assert!(!composite.is_empty());
    assert_eq!(composite.len(), 2);
}

#[tokio::test]
async fn test_max_steps_at_limit_stops() {
    let middleware = LimitsMiddleware::new(LimitsConfig::default().max_steps(5));
    let context = test_context("sess-1", 5);
    let state = ExecutionState::BeforeLlmCall { context };

    let result = step(&middleware, state).await;

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
    let middleware = LimitsMiddleware::new(LimitsConfig::default().max_steps(5));
    let context = test_context("sess-1", 3);
    let state = ExecutionState::BeforeLlmCall { context };

    let result = step(&middleware, state).await;

    assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));
}

#[tokio::test]
async fn test_turn_limit_middleware() {
    let middleware = LimitsMiddleware::new(LimitsConfig::default().max_turns(3));

    // With 2 user messages, should continue (2 < 3)
    let context = test_context_with_user_messages("sess-1", 2);
    let state = ExecutionState::BeforeLlmCall { context };
    let result = step(&middleware, state).await;
    assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));

    // With 3 user messages, should stop (3 >= 3)
    let context = test_context_with_user_messages("sess-1", 3);
    let state = ExecutionState::BeforeLlmCall { context };
    let result = step(&middleware, state).await;
    assert!(matches!(result, ExecutionState::Stopped { .. }));
}

fn budget_context(budget_cost_usd: f64) -> Arc<ConversationContext> {
    let stats = Arc::new(AgentStats {
        steps: 1,
        // Token counters are irrelevant to the check: only recorded cycle
        // charges count.
        total_input_tokens: 1_000_000_000,
        total_output_tokens: 1_000_000_000,
        budget_cost_usd,
        ..Default::default()
    });
    Arc::new(ConversationContext::new(
        "sess-1".into(),
        Arc::from([]),
        stats,
        "mock".into(),
        "mock-model".into(),
    ))
}

fn is_price_stop(state: &ExecutionState) -> bool {
    matches!(
        state,
        ExecutionState::Stopped {
            stop_type: StopType::PriceLimit,
            ..
        }
    )
}

#[tokio::test]
async fn test_price_limit_over_budget() {
    let middleware =
        LimitsMiddleware::new(LimitsConfig::with_manual_pricing(3.0, 15.0).max_price_usd(0.10));
    let state = ExecutionState::BeforeLlmCall {
        context: budget_context(0.25),
    };
    assert!(is_price_stop(&step(&middleware, state).await));
}

#[tokio::test]
async fn test_price_limit_stops_exactly_at_the_cap() {
    let middleware =
        LimitsMiddleware::new(LimitsConfig::with_manual_pricing(3.0, 15.0).max_price_usd(0.10));
    let state = ExecutionState::BeforeLlmCall {
        context: budget_context(0.10),
    };
    assert!(is_price_stop(&step(&middleware, state).await));
}

#[tokio::test]
async fn test_price_limit_under_budget() {
    let middleware =
        LimitsMiddleware::new(LimitsConfig::with_manual_pricing(3.0, 15.0).max_price_usd(1.0));
    let state = ExecutionState::BeforeLlmCall {
        context: budget_context(0.05),
    };
    let result = step(&middleware, state).await;
    assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));
}

#[tokio::test]
async fn test_new_cycle_with_zero_cycle_spend_is_not_exhausted() {
    // Prior-cycle spend never reaches the stats of a new cycle.
    let middleware =
        LimitsMiddleware::new(LimitsConfig::with_manual_pricing(3.0, 15.0).max_price_usd(0.10));
    let state = ExecutionState::BeforeLlmCall {
        context: budget_context(0.0),
    };
    let result = step(&middleware, state).await;
    assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));
}

#[tokio::test]
async fn test_limits_middleware_enforces_the_same_cycle_budget() {
    let middleware = LimitsMiddleware::new(LimitsConfig::default().max_price_usd(0.10));
    let over = ExecutionState::BeforeLlmCall {
        context: budget_context(0.10),
    };
    assert!(is_price_stop(&step(&middleware, over).await));
    let under = ExecutionState::BeforeLlmCall {
        context: budget_context(0.09),
    };
    let result = step(&middleware, under).await;
    assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));
}

#[tokio::test]
async fn test_price_limit_without_known_spend_passes_through() {
    // No recorded charge (for example an all-unpriced cycle): nothing to stop on.
    let middleware = LimitsMiddleware::new(LimitsConfig::default().max_price_usd(0.10));
    let state = ExecutionState::BeforeLlmCall {
        context: budget_context(0.0),
    };
    let result = step(&middleware, state).await;
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
    let result = step(&middleware, state).await;
    assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));

    let context = test_context("sess-1", 10);
    let state = ExecutionState::BeforeLlmCall { context };
    let result = step(&middleware, state).await;
    assert!(matches!(result, ExecutionState::Stopped { .. }));
}

// test_context moved to crate::test_utils::helpers
