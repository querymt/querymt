//! Calls the hooks of a run's middleware without running a loop, and returns
//! the state the loop would move to, so a test can assert on states.

use crate::agent::core::{AgentMode, SessionRuntime};
use crate::agent::execution::{step_state, stopped_state};
use crate::middleware::{
    CompositeDriver, EndVerdict, ExecutionState, Folded, HookCx, Middleware, ReplyVerdict, Result,
    Stop, StopVerdict, TurnInfo, TurnStack,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct HookRig {
    pub runtime: Arc<SessionRuntime>,
    pub changed: querymt_agent_index::DiffPaths,
    pub cancel: CancellationToken,
}

impl Default for HookRig {
    fn default() -> Self {
        Self {
            runtime: SessionRuntime::new(
                None,
                Default::default(),
                crate::agent::core::McpToolState::empty(),
            ),
            changed: Default::default(),
            cancel: CancellationToken::new(),
        }
    }
}

fn context_of(state: &ExecutionState) -> Arc<crate::middleware::ConversationContext> {
    state
        .context()
        .cloned()
        .expect("the state carries a context")
}

impl HookRig {
    pub fn new() -> Self {
        Self::default()
    }

    /// The stack of a run over `middleware`.
    pub fn stack(middleware: Vec<Arc<dyn Middleware>>) -> TurnStack {
        CompositeDriver::new(middleware)
            .begin_turn(&TurnInfo::new("test-session", AgentMode::Build))
    }

    /// The stack of a run over one middleware.
    pub fn stack_of(middleware: impl Middleware + 'static) -> TurnStack {
        Self::stack(vec![Arc::new(middleware)])
    }

    pub async fn turn_start(
        &self,
        stack: &mut TurnStack,
        state: ExecutionState,
    ) -> Result<ExecutionState> {
        let mut context = context_of(&state);
        let folded = stack
            .turn_start(
                &mut HookCx::new(&mut context, &self.runtime, &self.changed),
                &self.cancel,
            )
            .await?;
        Ok(step_state(folded, context))
    }

    pub async fn step_start(
        &self,
        stack: &mut TurnStack,
        state: ExecutionState,
    ) -> Result<ExecutionState> {
        let mut context = context_of(&state);
        let folded = stack
            .step_start(
                &mut HookCx::new(&mut context, &self.runtime, &self.changed),
                &self.cancel,
            )
            .await?;
        Ok(step_state(folded, context))
    }

    /// `state` is `AfterLlm`; a retry leads to `BeforeLlmCall` with the reply's usage counted.
    pub async fn after_llm(
        &self,
        stack: &mut TurnStack,
        state: ExecutionState,
    ) -> Result<ExecutionState> {
        let ExecutionState::AfterLlm {
            response,
            mut context,
        } = state
        else {
            panic!("after_llm needs an AfterLlm state");
        };
        let folded = stack
            .after_llm(
                &mut HookCx::new(&mut context, &self.runtime, &self.changed),
                &response,
                &self.cancel,
            )
            .await?;
        Ok(match folded {
            Folded::Cancelled => ExecutionState::Cancelled,
            Folded::Decided(ReplyVerdict::Proceed) => {
                ExecutionState::AfterLlm { response, context }
            }
            Folded::Decided(ReplyVerdict::Stop(stop)) => stopped_state(stop, context),
            Folded::Decided(ReplyVerdict::Retry) => {
                let stats = &mut Arc::make_mut(&mut context).stats;
                crate::middleware::apply_usage_to_stats(
                    Arc::make_mut(stats),
                    response.usage.as_ref(),
                );
                ExecutionState::BeforeLlmCall { context }
            }
        })
    }

    /// `state` is `Complete`.
    pub async fn turn_end(
        &self,
        stack: &mut TurnStack,
        state: ExecutionState,
    ) -> Result<ExecutionState> {
        let mut context = context_of(&state);
        let folded = stack
            .turn_end(
                &mut HookCx::new(&mut context, &self.runtime, &self.changed),
                &self.cancel,
            )
            .await?;
        Ok(match folded {
            Folded::Cancelled => ExecutionState::Cancelled,
            Folded::Decided(EndVerdict::Finish) => ExecutionState::Complete { context },
            Folded::Decided(EndVerdict::Continue) => ExecutionState::BeforeLlmCall { context },
            Folded::Decided(EndVerdict::Stop(stop)) => stopped_state(stop, context),
        })
    }

    /// `state` is `Stopped` with a context; a recovery leads to `BeforeLlmCall`,
    /// a pass leaves the state as it was.
    pub async fn stopped(
        &self,
        stack: &mut TurnStack,
        state: ExecutionState,
    ) -> Result<ExecutionState> {
        let ExecutionState::Stopped {
            message, stop_type, ..
        } = &state
        else {
            panic!("stopped needs a Stopped state");
        };
        let stop = Stop::new(*stop_type, message.clone());
        let mut context = context_of(&state);
        let folded = stack
            .stopped(
                &mut HookCx::new(&mut context, &self.runtime, &self.changed),
                &stop,
                &self.cancel,
            )
            .await?;
        Ok(match folded {
            Folded::Cancelled => ExecutionState::Cancelled,
            Folded::Decided(StopVerdict::Recover) => ExecutionState::BeforeLlmCall { context },
            Folded::Decided(StopVerdict::Pass) => state,
        })
    }
}
