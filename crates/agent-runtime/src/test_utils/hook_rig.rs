//! Calls the hooks of a run's middleware without running a loop, and returns
//! the state the loop would move to, so a test can assert on states. The rig
//! owns the run state a hook reads and writes (history, statistics, queued
//! notices, overrides), so a test arranges it up front and reads it back.

use crate::agent::core::{AgentMode, SessionRuntime};
use crate::agent::execution::turn::History;
use crate::agent::execution::{step_state, stopped_state};
use crate::middleware::{
    AgentStats, CompositeDriver, EndVerdict, ExecutionState, Folded, HookCx, HookInputs,
    LlmResponse, Middleware, ModelProfile, PendingNotice, ReplyVerdict, Result, Stop, StopVerdict,
    TurnInfo, TurnStack,
};
use querymt::chat::GenerationOptions;
use querymt_agent_core::events::StopType;
use querymt_agent_core::model::{AgentMessage, MessagePart};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct HookRig {
    pub runtime: Arc<SessionRuntime>,
    pub changed: querymt_agent_index::DiffPaths,
    pub cancel: CancellationToken,
    pub session_id: String,
    pub provider: String,
    pub model: String,
    pub mode: AgentMode,
    pub stats: AgentStats,
    pub pending: Vec<PendingNotice>,
    pub overrides: GenerationOptions,
    pub last_applied: GenerationOptions,
    pub profile: Arc<ModelProfile>,
    history: History,
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
            session_id: "test-session".into(),
            provider: "mock".into(),
            model: "mock-model".into(),
            mode: AgentMode::Build,
            stats: AgentStats::default(),
            pending: Vec::new(),
            overrides: GenerationOptions::NONE,
            last_applied: GenerationOptions::NONE,
            profile: Arc::new(ModelProfile::default()),
            history: History::default(),
        }
    }
}

impl HookRig {
    pub fn new() -> Self {
        Self::default()
    }

    /// A rig for `session_id` that has taken `steps` steps.
    pub fn at_step(session_id: &str, steps: usize) -> Self {
        let mut rig = Self {
            session_id: session_id.to_string(),
            ..Self::default()
        };
        rig.stats.steps = steps;
        rig
    }

    /// A rig whose history holds `count` user messages.
    pub fn with_user_messages(session_id: &str, count: usize) -> Self {
        let mut rig = Self::at_step(session_id, 0);
        rig.stats.turns = count;
        rig.history.replace(
            (0..count)
                .map(|index| {
                    let mut message =
                        AgentMessage::new(session_id.to_string(), querymt::chat::ChatRole::User);
                    message.parts.push(MessagePart::Text {
                        content: format!("User message {index}"),
                    });
                    message
                })
                .collect(),
        );
        rig
    }

    /// Replace the run's history.
    pub fn set_history(&mut self, messages: Vec<AgentMessage>) {
        self.history.replace(messages);
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

    /// The hook context over the rig's run state.
    pub fn cx(&mut self) -> HookCx<'_> {
        HookCx::new(HookInputs {
            session_id: &self.session_id,
            provider: &self.provider,
            model: &self.model,
            mode: self.mode,
            runtime: &self.runtime,
            history: &self.history,
            stats: &self.stats,
            profile: &self.profile,
            last_applied: self.last_applied,
            request_overrides: &mut self.overrides,
            pending_notices: &mut self.pending,
            changed: &self.changed,
        })
    }

    pub async fn turn_start(&mut self, stack: &mut TurnStack) -> Result<ExecutionState> {
        let cancel = self.cancel.clone();
        let folded = stack.turn_start(&mut self.cx(), &cancel).await?;
        Ok(step_state(folded))
    }

    pub async fn step_start(&mut self, stack: &mut TurnStack) -> Result<ExecutionState> {
        let cancel = self.cancel.clone();
        let folded = stack.step_start(&mut self.cx(), &cancel).await?;
        Ok(step_state(folded))
    }

    /// A retry leads to `BeforeLlmCall` with the reply's usage counted.
    pub async fn after_llm(
        &mut self,
        stack: &mut TurnStack,
        response: Arc<LlmResponse>,
    ) -> Result<ExecutionState> {
        let cancel = self.cancel.clone();
        let folded = stack.after_llm(&mut self.cx(), &response, &cancel).await?;
        Ok(match folded {
            Folded::Cancelled => ExecutionState::Cancelled,
            Folded::Decided(ReplyVerdict::Proceed) => ExecutionState::AfterLlm { response },
            Folded::Decided(ReplyVerdict::Stop(stop)) => stopped_state(stop),
            Folded::Decided(ReplyVerdict::Retry) => {
                crate::middleware::apply_usage_to_stats(&mut self.stats, response.usage.as_ref());
                ExecutionState::BeforeLlmCall
            }
        })
    }

    pub async fn turn_end(&mut self, stack: &mut TurnStack) -> Result<ExecutionState> {
        let cancel = self.cancel.clone();
        let folded = stack.turn_end(&mut self.cx(), &cancel).await?;
        Ok(match folded {
            Folded::Cancelled => ExecutionState::Cancelled,
            Folded::Decided(EndVerdict::Finish) => ExecutionState::Complete,
            Folded::Decided(EndVerdict::Continue) => ExecutionState::BeforeLlmCall,
            Folded::Decided(EndVerdict::Stop(stop)) => stopped_state(stop),
        })
    }

    /// A recovery leads to `BeforeLlmCall`; a pass leaves the stop as it was
    /// (the stack drops what the middleware queued while they were consulted).
    pub async fn stopped(
        &mut self,
        stack: &mut TurnStack,
        stop_type: StopType,
        message: &str,
    ) -> Result<ExecutionState> {
        let stop = Stop::new(stop_type, message);
        let cancel = self.cancel.clone();
        let folded = stack.stopped(&mut self.cx(), &stop, &cancel).await?;
        Ok(match folded {
            Folded::Cancelled => ExecutionState::Cancelled,
            Folded::Decided(StopVerdict::Recover) => ExecutionState::BeforeLlmCall,
            Folded::Decided(StopVerdict::Pass) => stopped_state(stop),
        })
    }
}

/// The hooks of a stack over a real run (`env` and `turn`), for tests that
/// drive the loop's steps and want the middleware to act on the run's state.
pub(crate) mod in_run {
    use super::*;
    use crate::agent::execution::turn::{Turn, TurnEnv};

    pub async fn step_start(
        stack: &mut TurnStack,
        env: &TurnEnv,
        turn: &mut Turn,
    ) -> Result<ExecutionState> {
        let folded = stack
            .step_start(&mut turn.hook_cx(env), &env.cancellation_token)
            .await?;
        Ok(step_state(folded))
    }

    /// `state` is `AfterLlm`; a retry leads to `BeforeLlmCall` with the reply's
    /// usage counted.
    pub async fn after_llm(
        stack: &mut TurnStack,
        env: &TurnEnv,
        turn: &mut Turn,
        state: ExecutionState,
    ) -> Result<ExecutionState> {
        let ExecutionState::AfterLlm { response } = state else {
            panic!("after_llm needs an AfterLlm state, got {}", state.name());
        };
        let folded = stack
            .after_llm(&mut turn.hook_cx(env), &response, &env.cancellation_token)
            .await?;
        Ok(match folded {
            Folded::Cancelled => ExecutionState::Cancelled,
            Folded::Decided(ReplyVerdict::Proceed) => ExecutionState::AfterLlm { response },
            Folded::Decided(ReplyVerdict::Stop(stop)) => stopped_state(stop),
            Folded::Decided(ReplyVerdict::Retry) => {
                crate::middleware::apply_usage_to_stats(&mut turn.stats, response.usage.as_ref());
                ExecutionState::BeforeLlmCall
            }
        })
    }

    /// `state` is `Stopped`; a recovery leads to `BeforeLlmCall`, a pass leaves
    /// the stop as it was.
    pub async fn stopped(
        stack: &mut TurnStack,
        env: &TurnEnv,
        turn: &mut Turn,
        state: ExecutionState,
    ) -> Result<ExecutionState> {
        let ExecutionState::Stopped {
            message, stop_type, ..
        } = &state
        else {
            panic!("stopped needs a Stopped state, got {}", state.name());
        };
        let stop = Stop::new(*stop_type, message.clone());
        let folded = stack
            .stopped(&mut turn.hook_cx(env), &stop, &env.cancellation_token)
            .await?;
        Ok(match folded {
            Folded::Cancelled => ExecutionState::Cancelled,
            Folded::Decided(StopVerdict::Recover) => ExecutionState::BeforeLlmCall,
            Folded::Decided(StopVerdict::Pass) => state,
        })
    }
}
