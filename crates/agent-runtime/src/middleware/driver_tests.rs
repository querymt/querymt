use crate::middleware::{
    CompositeDriver, EndVerdict, ExecutionState, LimitsConfig, LimitsMiddleware, Middleware,
    ReplyVerdict, StepVerdict, Stop, StopVerdict,
};
use crate::test_utils::{Act, HookRig, ScriptedMiddleware};
use querymt_agent_core::events::StopType;
use std::sync::Arc;

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

/// One step of `middleware` over the run `rig` holds, as the loop would take it.
async fn step(middleware: &LimitsMiddleware, mut rig: HookRig) -> ExecutionState {
    let mut stack = HookRig::stack_of(middleware.clone());
    rig.step_start(&mut stack).await.unwrap()
}

#[tokio::test]
async fn an_empty_stack_passes_every_stage() {
    let mut rig = HookRig::new();
    let mut stack = HookRig::stack(vec![]);

    let state = rig.step_start(&mut stack).await.unwrap();

    assert!(matches!(state, ExecutionState::BeforeLlmCall));
    assert!(stack.is_empty());
}

#[tokio::test]
async fn stop_before_a_model_request() {
    let stopper = ScriptedMiddleware::default().step_start([other_stop("enough")]);
    let mut stack = stack_of(&[&stopper]);

    let state = HookRig::new().step_start(&mut stack).await.unwrap();

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

    let state = HookRig::new().turn_start(&mut stack).await.unwrap();

    assert!(matches!(state, ExecutionState::Stopped { .. }));
    assert_eq!(stopper.count("turn_start"), 1);
}

#[tokio::test]
async fn earlier_stop_skips_later_middleware() {
    let first = ScriptedMiddleware::default().step_start([other_stop("first")]);
    let second = ScriptedMiddleware::default();
    let mut stack = stack_of(&[&first, &second]);

    let state = HookRig::new().step_start(&mut stack).await.unwrap();

    assert!(matches!(state, ExecutionState::Stopped { .. }));
    assert_eq!(second.count("step_start"), 0);
}

#[tokio::test]
async fn every_middleware_is_consulted_when_all_pass() {
    let first = ScriptedMiddleware::default();
    let second = ScriptedMiddleware::default();
    let mut stack = stack_of(&[&first, &second]);

    let state = HookRig::new().step_start(&mut stack).await.unwrap();

    assert!(matches!(state, ExecutionState::BeforeLlmCall));
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
    let mut rig = HookRig::new();

    let state = rig.after_llm(&mut stack, response).await.unwrap();

    assert!(
        matches!(state, ExecutionState::BeforeLlmCall),
        "a retry sends the request again"
    );
    assert_eq!(
        rig.stats.total_input_tokens, 100,
        "usage of the discarded reply counts"
    );
    assert_eq!(rig.stats.total_output_tokens, 40);
}

#[tokio::test]
async fn continuation_at_the_end_of_a_run() {
    let continuer = ScriptedMiddleware::default().turn_end([
        Act::Notice("one more thing", EndVerdict::Continue),
        Act::Say(EndVerdict::Finish),
    ]);
    let mut stack = stack_of(&[&continuer]);
    let mut rig = HookRig::new();

    let first = rig.turn_end(&mut stack).await.unwrap();
    let queued = rig.pending.clone();
    let second = rig.turn_end(&mut stack).await.unwrap();

    assert!(
        matches!(first, ExecutionState::BeforeLlmCall),
        "expected a continuation"
    );
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].content, "one more thing");
    assert!(matches!(second, ExecutionState::Complete));
}

#[tokio::test]
async fn stop_passed_on_by_every_middleware() {
    let first = ScriptedMiddleware::default().stopped([Act::Say(StopVerdict::Pass)]);
    let second = ScriptedMiddleware::default().stopped([Act::Say(StopVerdict::Pass)]);
    let mut stack = stack_of(&[&first, &second]);

    let state = HookRig::new()
        .stopped(&mut stack, StopType::ModelTokenLimit, "cut off")
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
        .stopped(&mut stack, StopType::ModelTokenLimit, "cut off")
        .await
        .unwrap();

    assert!(matches!(state, ExecutionState::BeforeLlmCall));
    assert_eq!(never.count("stopped"), 0);
}

#[tokio::test]
async fn notices_queued_before_a_stop_survive_a_recovery() {
    let queues =
        ScriptedMiddleware::default().step_start([Act::Notice("kept", StepVerdict::Proceed)]);
    let stops = ScriptedMiddleware::default().step_start([other_stop("stop")]);
    let recovers = ScriptedMiddleware::default().stopped([Act::Say(StopVerdict::Recover)]);
    let mut stack = stack_of(&[&queues, &stops, &recovers]);
    let mut rig = HookRig::new();

    rig.step_start(&mut stack).await.unwrap();
    let recovered = rig
        .stopped(&mut stack, StopType::Other, "stop")
        .await
        .unwrap();

    assert!(
        matches!(recovered, ExecutionState::BeforeLlmCall),
        "expected a recovery"
    );
    assert_eq!(rig.pending.len(), 1);
    assert_eq!(rig.pending[0].content, "kept");
}

#[tokio::test]
async fn notices_queued_before_a_published_stop_are_not_stored() {
    let queues =
        ScriptedMiddleware::default().step_start([Act::Notice("lost", StepVerdict::Proceed)]);
    let stops = ScriptedMiddleware::default().step_start([other_stop("stop")]);
    let passes = ScriptedMiddleware::default().stopped([Act::Say(StopVerdict::Pass)]);
    let mut stack = stack_of(&[&queues, &stops, &passes]);
    let mut rig = HookRig::new();

    rig.step_start(&mut stack).await.unwrap();
    let queued_before = rig.pending.len();
    let published = rig
        .stopped(&mut stack, StopType::Other, "stop")
        .await
        .unwrap();

    // The stop is published as it was; the notice queued before it stays on
    // the run, which ends without storing it.
    assert!(matches!(published, ExecutionState::Stopped { .. }));
    assert_eq!(queued_before, 1);
}

#[tokio::test]
async fn cancel_while_a_middleware_is_running() {
    let hangs = ScriptedMiddleware::default().step_start([Act::Hang]);
    let mut stack = stack_of(&[&hangs]);
    let mut rig = HookRig::new();
    let cancel = rig.cancel.clone();
    let task = tokio::spawn(async move { rig.step_start(&mut stack).await });

    while hangs.count("step_start") == 0 {
        tokio::task::yield_now().await;
    }
    cancel.cancel();

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
    let mut rig = HookRig::new();
    rig.cancel.cancel();

    let state = rig.step_start(&mut stack).await.unwrap();

    assert!(matches!(state, ExecutionState::Cancelled));
    assert_eq!(counter.count("step_start"), 0);
}

#[tokio::test]
async fn middleware_error_propagates() {
    let fails = ScriptedMiddleware::default().step_start([Act::Fail]);
    let mut stack = stack_of(&[&ScriptedMiddleware::default(), &fails]);

    let result = HookRig::new().step_start(&mut stack).await;

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

    let result = step(&middleware, HookRig::at_step("sess-1", 5)).await;

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

    let result = step(&middleware, HookRig::at_step("sess-1", 3)).await;

    assert!(matches!(result, ExecutionState::BeforeLlmCall));
}

#[tokio::test]
async fn test_turn_limit_middleware() {
    let middleware = LimitsMiddleware::new(LimitsConfig::default().max_turns(3));

    // With 2 user messages, should continue (2 < 3)
    let result = step(&middleware, HookRig::with_user_messages("sess-1", 2)).await;
    assert!(matches!(result, ExecutionState::BeforeLlmCall));

    // With 3 user messages, should stop (3 >= 3)
    let result = step(&middleware, HookRig::with_user_messages("sess-1", 3)).await;
    assert!(matches!(result, ExecutionState::Stopped { .. }));
}

fn budget_rig(budget_cost_usd: f64) -> HookRig {
    let mut rig = HookRig::at_step("sess-1", 1);
    // Token counters are irrelevant to the check: only recorded cycle
    // charges count.
    rig.stats.total_input_tokens = 1_000_000_000;
    rig.stats.total_output_tokens = 1_000_000_000;
    rig.stats.budget_cost_usd = budget_cost_usd;
    rig
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
    assert!(is_price_stop(&step(&middleware, budget_rig(0.25)).await));
}

#[tokio::test]
async fn test_price_limit_stops_exactly_at_the_cap() {
    let middleware =
        LimitsMiddleware::new(LimitsConfig::with_manual_pricing(3.0, 15.0).max_price_usd(0.10));
    assert!(is_price_stop(&step(&middleware, budget_rig(0.10)).await));
}

#[tokio::test]
async fn test_price_limit_under_budget() {
    let middleware =
        LimitsMiddleware::new(LimitsConfig::with_manual_pricing(3.0, 15.0).max_price_usd(1.0));
    let result = step(&middleware, budget_rig(0.05)).await;
    assert!(matches!(result, ExecutionState::BeforeLlmCall));
}

#[tokio::test]
async fn test_new_cycle_with_zero_cycle_spend_is_not_exhausted() {
    // Prior-cycle spend never reaches the stats of a new cycle.
    let middleware =
        LimitsMiddleware::new(LimitsConfig::with_manual_pricing(3.0, 15.0).max_price_usd(0.10));
    let result = step(&middleware, budget_rig(0.0)).await;
    assert!(matches!(result, ExecutionState::BeforeLlmCall));
}

#[tokio::test]
async fn test_limits_middleware_enforces_the_same_cycle_budget() {
    let middleware = LimitsMiddleware::new(LimitsConfig::default().max_price_usd(0.10));
    assert!(is_price_stop(&step(&middleware, budget_rig(0.10)).await));
    let result = step(&middleware, budget_rig(0.09)).await;
    assert!(matches!(result, ExecutionState::BeforeLlmCall));
}

#[tokio::test]
async fn test_price_limit_without_known_spend_passes_through() {
    // No recorded charge (for example an all-unpriced cycle): nothing to stop on.
    let middleware = LimitsMiddleware::new(LimitsConfig::default().max_price_usd(0.10));
    let result = step(&middleware, budget_rig(0.0)).await;
    assert!(matches!(result, ExecutionState::BeforeLlmCall));
}

#[tokio::test]
async fn test_limits_middleware_combined() {
    let config = LimitsConfig::with_manual_pricing(3.0, 15.0)
        .max_steps(10)
        .max_turns(5)
        .max_price_usd(1.0);
    let middleware = LimitsMiddleware::new(config);

    let result = step(&middleware, HookRig::at_step("sess-1", 2)).await;
    assert!(matches!(result, ExecutionState::BeforeLlmCall));

    let result = step(&middleware, HookRig::at_step("sess-1", 10)).await;
    assert!(matches!(result, ExecutionState::Stopped { .. }));
}
