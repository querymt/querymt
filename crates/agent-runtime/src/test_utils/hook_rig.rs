//! Calls the hooks of a run's middleware without running a loop, and returns
//! the step the loop would take, so a test can assert on steps. The rig
//! owns the run state a hook reads and writes (history, statistics, queued
//! notices, overrides), so a test arranges it up front and reads it back.

use crate::agent::core::{AgentMode, SessionRuntime};
use crate::agent::execution::stage::{after_step, cancelled};
use crate::agent::execution::turn::History;
use crate::agent::execution::{Stage, Step};
use crate::middleware::{
    AgentStats, CompositeDriver, EndVerdict, Folded, HookCx, HookInputs, LlmResponse, Middleware,
    ModelProfile, PendingNotice, ReplyVerdict, Result, Stop, StopVerdict, TurnInfo, TurnStack,
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

    pub(crate) async fn turn_start(&mut self, stack: &mut TurnStack) -> Result<Step> {
        let cancel = self.cancel.clone();
        let folded = stack.turn_start(&mut self.cx(), &cancel).await?;
        Ok(after_step(folded))
    }

    pub(crate) async fn step_start(&mut self, stack: &mut TurnStack) -> Result<Step> {
        let cancel = self.cancel.clone();
        let folded = stack.step_start(&mut self.cx(), &cancel).await?;
        Ok(after_step(folded))
    }

    /// A retry leads to `BeforeLlmCall` with the reply's usage counted.
    pub(crate) async fn after_llm(
        &mut self,
        stack: &mut TurnStack,
        response: Box<LlmResponse>,
    ) -> Result<Step> {
        let cancel = self.cancel.clone();
        let folded = stack.after_llm(&mut self.cx(), &response, &cancel).await?;
        Ok(match folded {
            Folded::Cancelled => cancelled(),
            Folded::Decided(ReplyVerdict::Proceed) => Step::Continue(Stage::AfterReply(response)),
            Folded::Decided(ReplyVerdict::Stop(stop)) => Step::Continue(Stage::Stopping(stop)),
            Folded::Decided(ReplyVerdict::Retry) => {
                crate::middleware::apply_usage_to_stats(&mut self.stats, response.usage.as_ref());
                Step::Continue(Stage::BeforeRequest)
            }
        })
    }

    pub(crate) async fn turn_end(&mut self, stack: &mut TurnStack) -> Result<Step> {
        let cancel = self.cancel.clone();
        let folded = stack.turn_end(&mut self.cx(), &cancel).await?;
        Ok(match folded {
            Folded::Cancelled => cancelled(),
            Folded::Decided(EndVerdict::Finish) => Step::Continue(Stage::Ending),
            Folded::Decided(EndVerdict::Continue) => Step::Continue(Stage::BeforeRequest),
            Folded::Decided(EndVerdict::Stop(stop)) => Step::Continue(Stage::Stopping(stop)),
        })
    }

    /// A recovery leads to `BeforeLlmCall`; a pass leaves the stop as it was
    /// (the stack drops what the middleware queued while they were consulted).
    pub(crate) async fn stopped(
        &mut self,
        stack: &mut TurnStack,
        stop_type: StopType,
        message: &str,
    ) -> Result<Step> {
        let stop = Stop::new(stop_type, message);
        let cancel = self.cancel.clone();
        let folded = stack.stopped(&mut self.cx(), &stop, &cancel).await?;
        Ok(match folded {
            Folded::Cancelled => cancelled(),
            Folded::Decided(StopVerdict::Recover) => Step::Continue(Stage::BeforeRequest),
            Folded::Decided(StopVerdict::Pass) => Step::Continue(Stage::Stopping(stop)),
        })
    }
}

/// The hooks of a stack over a real run (`env` and `turn`), for tests that
/// drive the loop's steps and want the middleware to act on the run's state.
pub(crate) mod in_run {
    use super::*;
    use crate::agent::execution::turn::{Turn, TurnEnv};

    pub(crate) async fn step_start(
        stack: &mut TurnStack,
        env: &TurnEnv,
        turn: &mut Turn,
    ) -> Result<Step> {
        let folded = stack
            .step_start(&mut turn.hook_cx(env), &env.cancellation_token)
            .await?;
        Ok(after_step(folded))
    }

    /// `state` is `AfterReply`; a retry leads to `BeforeLlmCall` with the reply's
    /// usage counted.
    pub(crate) async fn after_llm(
        stack: &mut TurnStack,
        env: &TurnEnv,
        turn: &mut Turn,
        state: Step,
    ) -> Result<Step> {
        let Step::Continue(Stage::AfterReply(response)) = state else {
            panic!("after_llm needs an AfterReply stage, got {state:?}");
        };
        let folded = stack
            .after_llm(&mut turn.hook_cx(env), &response, &env.cancellation_token)
            .await?;
        Ok(match folded {
            Folded::Cancelled => cancelled(),
            Folded::Decided(ReplyVerdict::Proceed) => Step::Continue(Stage::AfterReply(response)),
            Folded::Decided(ReplyVerdict::Stop(stop)) => Step::Continue(Stage::Stopping(stop)),
            Folded::Decided(ReplyVerdict::Retry) => {
                crate::middleware::apply_usage_to_stats(&mut turn.stats, response.usage.as_ref());
                Step::Continue(Stage::BeforeRequest)
            }
        })
    }

    /// `state` is `Stopping`; a recovery leads to `BeforeLlmCall`, a pass leaves
    /// the stop as it was.
    pub(crate) async fn stopped(
        stack: &mut TurnStack,
        env: &TurnEnv,
        turn: &mut Turn,
        state: Step,
    ) -> Result<Step> {
        let Step::Continue(Stage::Stopping(stop)) = &state else {
            panic!("stopped needs a Stopping stage, got {state:?}");
        };
        let folded = stack
            .stopped(&mut turn.hook_cx(env), stop, &env.cancellation_token)
            .await?;
        Ok(match folded {
            Folded::Cancelled => cancelled(),
            Folded::Decided(StopVerdict::Recover) => Step::Continue(Stage::BeforeRequest),
            Folded::Decided(StopVerdict::Pass) => state,
        })
    }
}
