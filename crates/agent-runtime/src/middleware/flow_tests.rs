use crate::middleware::{
    AgentStats, ConversationContext, ExecutionState, LlmResponse, Middleware, ReplyVerdict,
    StepVerdict, Stop,
};
use crate::test_utils::{Act, HookRig, ScriptedMiddleware, test_context};
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
    let rig = HookRig::new();
    let context = test_context("sess-1", 0);

    rig.turn_start(
        &mut stack,
        ExecutionState::BeforeLlmCall {
            context: context.clone(),
        },
    )
    .await
    .unwrap();
    rig.step_start(
        &mut stack,
        ExecutionState::BeforeLlmCall {
            context: context.clone(),
        },
    )
    .await
    .unwrap();
    rig.after_llm(
        &mut stack,
        ExecutionState::AfterLlm {
            response: response(),
            context: context.clone(),
        },
    )
    .await
    .unwrap();

    let hooks: Vec<_> = recorder.calls().into_iter().map(|(hook, _)| hook).collect();
    assert_eq!(hooks, ["turn_start", "step_start", "after_llm"]);
}

#[tokio::test]
async fn test_message_injection_flow() {
    let injector = ScriptedMiddleware::default()
        .turn_start([Act::Notice("Remember to be helpful", StepVerdict::Proceed)]);
    let mut stack = stack_of(&[&injector]);

    let result = HookRig::new()
        .turn_start(
            &mut stack,
            ExecutionState::BeforeLlmCall {
                context: test_context("sess-1", 0),
            },
        )
        .await
        .unwrap();

    let ExecutionState::BeforeLlmCall { context } = result else {
        panic!("Expected BeforeLlmCall state");
    };
    assert_eq!(context.pending_notices.len(), 1);
    assert!(
        context.pending_notices[0]
            .content
            .contains("Remember to be helpful")
    );
}

#[tokio::test]
async fn test_multiple_middleware_interaction() {
    let recorder = ScriptedMiddleware::default();
    let injector =
        ScriptedMiddleware::default().turn_start([Act::Notice("injected", StepVerdict::Proceed)]);
    let mut stack = stack_of(&[&recorder, &injector]);

    let result = HookRig::new()
        .turn_start(
            &mut stack,
            ExecutionState::BeforeLlmCall {
                context: test_context("sess-1", 0),
            },
        )
        .await
        .unwrap();

    assert_eq!(recorder.calls().len(), 1);
    let ExecutionState::BeforeLlmCall { context } = result else {
        panic!("Expected BeforeLlmCall state");
    };
    assert_eq!(context.pending_notices.len(), 1);
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
    let rig = HookRig::new();
    let context = test_context("sess-1", 0);
    let before = || ExecutionState::BeforeLlmCall {
        context: context.clone(),
    };

    let result = rig.turn_start(&mut stack, before()).await.unwrap();
    assert!(matches!(result, ExecutionState::Stopped { .. }));

    let result = rig.step_start(&mut stack, before()).await.unwrap();
    assert!(matches!(result, ExecutionState::Stopped { .. }));

    let result = rig
        .after_llm(
            &mut stack,
            ExecutionState::AfterLlm {
                response: response(),
                context: context.clone(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(result, ExecutionState::Stopped { .. }));
}

#[tokio::test]
async fn test_stats_propagate_through_middleware() {
    let mut stack = stack_of(&[&ScriptedMiddleware::default()]);
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

    let result = HookRig::new()
        .turn_start(&mut stack, ExecutionState::BeforeLlmCall { context })
        .await
        .unwrap();

    let ExecutionState::BeforeLlmCall { context } = result else {
        panic!("Expected BeforeLlmCall state");
    };
    assert_eq!(context.stats.steps, 42);
    assert_eq!(context.stats.total_input_tokens, 1000);
}

#[tokio::test]
async fn test_context_immutability() {
    let injector = ScriptedMiddleware::default()
        .turn_start([Act::Notice("new message", StepVerdict::Proceed)]);
    let mut stack = stack_of(&[&injector]);
    let original = test_context("sess-1", 0);
    let original_len = original.messages.len();

    let result = HookRig::new()
        .turn_start(
            &mut stack,
            ExecutionState::BeforeLlmCall {
                context: original.clone(),
            },
        )
        .await
        .unwrap();

    assert_eq!(original.messages.len(), original_len);
    assert!(original.pending_notices.is_empty());
    let ExecutionState::BeforeLlmCall { context } = result else {
        panic!("Expected BeforeLlmCall state");
    };
    assert_eq!(context.pending_notices.len(), 1);
}
