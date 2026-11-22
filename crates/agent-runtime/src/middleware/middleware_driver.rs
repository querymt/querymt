//! The middleware traits, in their own file so that edits elsewhere in the
//! module do not move their definitions (a shifted `dyn` trait span costs about
//! a minute of rebuild in the crates above).
//!
//! A [`Middleware`] is agent-scoped: it holds configuration and shared services.
//! For every run it creates a [`TurnMiddleware`], which owns all state of that
//! run and is dropped when the run ends. Nothing a middleware keeps while a run
//! executes is visible to another session or to a later run.

use super::driver::{PreparedBatch, PreparedCallVerdict};
use crate::agent::core::{AgentMode, SessionRuntime};
use crate::agent::execution::turn::History;
use crate::middleware::{AgentStats, LlmResponse, ModelProfile, PendingNotice, Result};
use async_trait::async_trait;
use querymt::chat::GenerationOptions;
use querymt_agent_core::events::{SessionLimits, StopType};
use querymt_agent_core::model::{AgentMessage, NoticeSource};
use querymt_agent_index::DiffPaths;
use std::sync::Arc;

/// A reason for ending the run, with the type that clients see.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stop {
    /// Why the run stops.
    pub stop_type: StopType,
    /// Human-readable explanation, published with the stop.
    pub message: Arc<str>,
}

impl Stop {
    /// A stop of `stop_type` explained by `message`.
    pub fn new(stop_type: StopType, message: impl Into<Arc<str>>) -> Self {
        Self {
            stop_type,
            message: message.into(),
        }
    }
}

/// Decision of a hook that runs before a model request.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
#[non_exhaustive]
pub enum StepVerdict {
    /// Let the run go on.
    Proceed,
    /// End the run, unless a middleware recovers the stop.
    Stop(Stop),
}

/// Decision of a hook that runs after a model reply.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
#[non_exhaustive]
pub enum ReplyVerdict {
    /// Use the reply.
    Proceed,
    /// End the run, unless a middleware recovers the stop.
    Stop(Stop),
    /// Discard the reply and send the request again. The reply's token usage
    /// is counted; the overrides set through [`HookCx::set_request_overrides`]
    /// apply to the new request.
    Retry,
}

/// Decision of a hook that runs when the model has ended its turn.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
#[non_exhaustive]
pub enum EndVerdict {
    /// Let the run end.
    Finish,
    /// Send another model request in the same run.
    Continue,
    /// End the run with a stop, unless a middleware recovers it.
    Stop(Stop),
}

/// Decision of a hook that runs when the run is about to end on a stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
#[non_exhaustive]
pub enum StopVerdict {
    /// Leave the stop to the next middleware and, finally, to publication.
    Pass,
    /// Replace the stop with another model request.
    Recover,
}

/// What a middleware is told about a run when it begins.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct TurnInfo<'a> {
    /// The session the run belongs to.
    pub session_id: &'a str,
    /// The session's agent mode when the run started.
    pub mode: AgentMode,
}

impl<'a> TurnInfo<'a> {
    /// Info for a run of `session_id` in `mode`.
    pub fn new(session_id: &'a str, mode: AgentMode) -> Self {
        Self { session_id, mode }
    }
}

/// What a hook may read of the run and the two things it may write: notices
/// for the next model request and generation overrides for it.
///
/// Writes land in the run in the order middleware are consulted.
pub struct HookCx<'a> {
    inputs: HookInputs<'a>,
}

/// What a hook is handed: borrows of the run's state, read-only except for
/// the two things a hook may write.
pub(crate) struct HookInputs<'a> {
    pub session_id: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub mode: AgentMode,
    pub runtime: &'a Arc<SessionRuntime>,
    pub history: &'a History,
    pub stats: &'a AgentStats,
    pub profile: &'a ModelProfile,
    pub last_applied: GenerationOptions,
    pub request_overrides: &'a mut GenerationOptions,
    pub pending_notices: &'a mut Vec<PendingNotice>,
    pub changed: &'a DiffPaths,
}

/// The state of a hook's two writes at one point of a stage.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HookCheckpoint {
    queued: usize,
    overrides: GenerationOptions,
}

impl<'a> HookCx<'a> {
    pub(crate) fn new(inputs: HookInputs<'a>) -> Self {
        Self { inputs }
    }

    /// The session the run belongs to.
    pub fn session_id(&self) -> &str {
        self.inputs.session_id
    }

    /// The conversation as the session stores it.
    pub fn history(&self) -> &[AgentMessage] {
        self.inputs.history.as_slice()
    }

    /// Usage and cost counters of the run so far.
    pub fn stats(&self) -> &AgentStats {
        self.inputs.stats
    }

    /// The provider of the session's model.
    pub fn provider(&self) -> &str {
        self.inputs.provider
    }

    /// The session's model.
    pub fn model(&self) -> &str {
        self.inputs.model
    }

    /// Facts about the session's model: output caps, capabilities, effort.
    pub fn profile(&self) -> &ModelProfile {
        self.inputs.profile
    }

    /// The session's agent mode.
    pub fn mode(&self) -> AgentMode {
        self.inputs.mode
    }

    /// The options the most recent model request was sent with.
    pub fn last_applied(&self) -> GenerationOptions {
        self.inputs.last_applied
    }

    /// The overrides set for the next request so far.
    pub fn request_overrides(&self) -> GenerationOptions {
        *self.inputs.request_overrides
    }

    /// Content of the most recent mode notice, counting notices queued but not
    /// yet stored.
    pub fn last_mode_notice(&self) -> Option<&str> {
        self.inputs
            .pending_notices
            .iter()
            .rev()
            .find(|notice| notice.source == NoticeSource::AgentMode)
            .map(|notice| notice.content.as_str())
            .or_else(|| self.inputs.history.last_mode_notice())
    }

    /// Notices queued for the next request and not yet stored.
    pub fn pending_notices(&self) -> &[PendingNotice] {
        self.inputs.pending_notices
    }

    /// The session's shared runtime state (working directory, indexes).
    pub fn runtime(&self) -> &Arc<SessionRuntime> {
        self.inputs.runtime
    }

    /// The files the run's tool calls changed so far.
    pub fn changed_paths(&self) -> &DiffPaths {
        self.inputs.changed
    }

    /// What a rollback restores: the writes a hook may make.
    pub(crate) fn checkpoint(&self) -> HookCheckpoint {
        HookCheckpoint {
            queued: self.inputs.pending_notices.len(),
            overrides: *self.inputs.request_overrides,
        }
    }

    /// Drops the notices queued and the overrides set since `checkpoint`.
    pub(crate) fn rollback(&mut self, checkpoint: HookCheckpoint) {
        self.inputs.pending_notices.truncate(checkpoint.queued);
        *self.inputs.request_overrides = checkpoint.overrides;
    }

    /// Queue a notice. It is stored before the next model request and is sent
    /// to the model as user text.
    pub fn queue_notice(&mut self, source: NoticeSource, content: impl Into<String>) {
        self.inputs.pending_notices.push(PendingNotice {
            source,
            content: content.into(),
        });
    }

    /// Set the generation overrides of the next model request, replacing any
    /// set earlier. They apply to that one request.
    pub fn set_request_overrides(&mut self, overrides: GenerationOptions) {
        *self.inputs.request_overrides = overrides;
    }
}

/// Agent-scoped middleware: configuration and shared services only.
///
/// State that belongs to one run lives in the [`TurnMiddleware`] returned by
/// [`begin_turn`](Middleware::begin_turn).
pub trait Middleware: Send + Sync {
    /// A human-readable name, used in logs and spans.
    fn name(&self) -> &'static str;

    /// Create the state of one run. Called once when a run starts.
    fn begin_turn(&self, turn: &TurnInfo<'_>) -> Box<dyn TurnMiddleware>;

    /// Called once the agent's final tool registry and event sink are known.
    /// Tool-aware middleware should use this registry so they agree with the
    /// scheduler about what tools exist; middleware that surfaces warnings to
    /// UIs can keep the sink.
    fn bind_agent(
        &self,
        _registry: &Arc<crate::tools::ToolRegistry>,
        _events: &Arc<querymt_agent_services::event_sink::EventSink>,
    ) {
    }

    /// Session limits this middleware enforces, if any.
    fn limits(&self) -> Option<SessionLimits> {
        None
    }

    /// Manual budget pricing this middleware wants snapshotted onto every paid
    /// attempt, if any. It is valued separately from reported API spend and
    /// never replaces it.
    fn budget_pricing(&self) -> Option<querymt::providers::ModelPricing> {
        None
    }
}

/// The state and hooks of one middleware for one run.
///
/// Hooks run in the order the middleware are configured; the first decisive
/// verdict ends the stage for the middleware after it. A hook future is
/// dropped when the run is cancelled, so a hook must leave `self` usable at
/// every `.await`; this is trivially true because the instance is dropped
/// with the run.
#[async_trait]
pub trait TurnMiddleware: Send {
    /// Runs once when the run starts, before its first step.
    async fn on_turn_start(&mut self, _cx: &mut HookCx<'_>) -> Result<StepVerdict> {
        Ok(StepVerdict::Proceed)
    }

    /// Runs before each model request, tool-loop continuations included.
    async fn on_step_start(&mut self, _cx: &mut HookCx<'_>) -> Result<StepVerdict> {
        Ok(StepVerdict::Proceed)
    }

    /// Runs after a model reply, before it is stored.
    async fn on_after_llm(
        &mut self,
        _cx: &mut HookCx<'_>,
        _reply: &LlmResponse,
    ) -> Result<ReplyVerdict> {
        Ok(ReplyVerdict::Proceed)
    }

    /// Reviews calls after argument preparation (normalisation, pre-tool
    /// hooks, validation) and before scheduling. Conflict-aware middleware
    /// must decide from `batch`'s effective arguments and accesses, never from
    /// raw model output. Returns one verdict per call in `batch.calls`, in
    /// order.
    async fn on_prepared_tool_calls(
        &mut self,
        _cx: &mut HookCx<'_>,
        batch: &PreparedBatch<'_>,
    ) -> Result<Vec<PreparedCallVerdict>> {
        Ok(batch
            .calls
            .iter()
            .map(|_| PreparedCallVerdict::Proceed)
            .collect())
    }

    /// Runs once when the model has ended its turn and the run is about to
    /// finish. A middleware can ask for another request, for example a
    /// post-run review.
    async fn on_turn_end(&mut self, _cx: &mut HookCx<'_>) -> Result<EndVerdict> {
        Ok(EndVerdict::Finish)
    }

    /// Runs when the run is about to end on `stop`, before the stop is
    /// published. A middleware may recover by queueing what the next request
    /// needs and returning [`StopVerdict::Recover`].
    async fn on_stopped(&mut self, _cx: &mut HookCx<'_>, _stop: &Stop) -> Result<StopVerdict> {
        Ok(StopVerdict::Pass)
    }
}
