use crate::middleware::{ExecutionState, LlmResponse, Middleware, ReplyVerdict, StepVerdict, Stop};
use crate::test_utils::{Act, HookRig, ScriptedMiddleware};
use querymt::chat::FinishReason;
use querymt_agent_core::events::StopType;
use std::sync::Arc;

fn stack_of(middleware: &[&ScriptedMiddleware]) -> crate::middleware::TurnStack {
    HookRig::stack(
        middleware
            .iter()
            .map(|m| Arc::new((*m).clone()) as Arc<dyn Middleware>)
            .collect(),
    )
}

fn response() -> Arc<LlmResponse> {
    Arc::new(LlmResponse::new(
        "".to_string(),
        vec![],
        None,
        Some(FinishReason::Stop),
    ))
}

#[tokio::test]
async fn every_hook_is_called_in_run_order() {
    let recorder = ScriptedMiddleware::default();
    let mut stack = stack_of(&[&recorder]);
    let mut rig = HookRig::at_step("sess-1", 0);

    rig.turn_start(&mut stack).await.unwrap();
    rig.step_start(&mut stack).await.unwrap();
    rig.after_llm(&mut stack, response()).await.unwrap();

    let hooks: Vec<_> = recorder.calls().into_iter().map(|(hook, _)| hook).collect();
    assert_eq!(hooks, ["turn_start", "step_start", "after_llm"]);
}

#[tokio::test]
async fn test_message_injection_flow() {
    let injector = ScriptedMiddleware::default()
        .turn_start([Act::Notice("Remember to be helpful", StepVerdict::Proceed)]);
    let mut stack = stack_of(&[&injector]);
    let mut rig = HookRig::at_step("sess-1", 0);

    let result = rig.turn_start(&mut stack).await.unwrap();

    assert!(matches!(result, ExecutionState::BeforeLlmCall));
    assert_eq!(rig.pending.len(), 1);
    assert!(rig.pending[0].content.contains("Remember to be helpful"));
}

#[tokio::test]
async fn test_multiple_middleware_interaction() {
    let recorder = ScriptedMiddleware::default();
    let injector =
        ScriptedMiddleware::default().turn_start([Act::Notice("injected", StepVerdict::Proceed)]);
    let mut stack = stack_of(&[&recorder, &injector]);
    let mut rig = HookRig::at_step("sess-1", 0);

    let result = rig.turn_start(&mut stack).await.unwrap();

    assert_eq!(recorder.calls().len(), 1);
    assert!(matches!(result, ExecutionState::BeforeLlmCall));
    assert_eq!(rig.pending.len(), 1);
}

#[tokio::test]
async fn test_middleware_can_transform_to_stopped() {
    let stopper = ScriptedMiddleware::default()
        .turn_start([Act::Say(StepVerdict::Stop(Stop::new(
            StopType::Other,
            "stop",
        )))])
        .step_start([Act::Say(StepVerdict::Stop(Stop::new(
            StopType::Other,
            "stop",
        )))])
        .after_llm([Act::Say(ReplyVerdict::Stop(Stop::new(
            StopType::Other,
            "stop",
        )))]);
    let mut stack = stack_of(&[&stopper]);
    let mut rig = HookRig::at_step("sess-1", 0);

    let result = rig.turn_start(&mut stack).await.unwrap();
    assert!(matches!(result, ExecutionState::Stopped { .. }));

    let result = rig.step_start(&mut stack).await.unwrap();
    assert!(matches!(result, ExecutionState::Stopped { .. }));

    let result = rig.after_llm(&mut stack, response()).await.unwrap();
    assert!(matches!(result, ExecutionState::Stopped { .. }));
}

#[tokio::test]
async fn test_stats_propagate_through_middleware() {
    let mut stack = stack_of(&[&ScriptedMiddleware::default()]);
    let mut rig = HookRig::at_step("sess-1", 42);
    rig.stats.total_input_tokens = 1000;
    rig.stats.total_output_tokens = 500;
    rig.stats.context_tokens = 1500;

    let result = rig.turn_start(&mut stack).await.unwrap();

    assert!(matches!(result, ExecutionState::BeforeLlmCall));
    assert_eq!(rig.stats.steps, 42);
    assert_eq!(rig.stats.total_input_tokens, 1000);
}

#[tokio::test]
async fn a_hook_queues_notices_without_touching_the_history() {
    let injector = ScriptedMiddleware::default()
        .turn_start([Act::Notice("new message", StepVerdict::Proceed)]);
    let mut stack = stack_of(&[&injector]);
    let mut rig = HookRig::with_user_messages("sess-1", 2);
    assert!(rig.pending.is_empty());

    rig.turn_start(&mut stack).await.unwrap();

    assert_eq!(rig.cx().history().len(), 2, "stored only at the request");
    assert_eq!(rig.pending.len(), 1);
}
