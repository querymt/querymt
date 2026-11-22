use crate::middleware::middleware_driver::{
    EndVerdict, HookCx, Middleware, ReplyVerdict, StepVerdict, Stop, StopVerdict, TurnInfo,
    TurnMiddleware,
};
use crate::middleware::{LlmResponse, MiddlewareError, Result, ToolCall, ToolResult};
use crate::tools::FileAccess;
use querymt_agent_core::events::SessionLimits;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span};

/// One prepared tool call as seen by middleware review.
///
/// `args` are the *effective* arguments (normalised and hook-adjusted) and
/// `access` is the file access the tool declared for exactly those arguments.
/// The model-emitted arguments on `call` must not be used for conflict
/// decisions.
#[derive(Debug, Clone, Copy)]
pub struct PreparedToolCall<'a> {
    pub call: &'a ToolCall,
    pub args: &'a serde_json::Value,
    pub access: &'a FileAccess,
}

/// The prepared, executable calls of one scheduling segment, in model order.
#[derive(Debug, Clone, Copy)]
pub struct PreparedBatch<'a> {
    pub session_id: &'a str,
    pub cwd: Option<&'a Path>,
    pub calls: &'a [PreparedToolCall<'a>],
}

/// Outcome of reviewing one prepared call.
#[derive(Debug)]
pub enum PreparedCallVerdict {
    /// The call may be scheduled and executed.
    Proceed,
    /// The call is not executed; this is its final result.
    Reject(Box<ToolResult>),
}

/// The agent's middleware, in configured order. Holds no run state: a run
/// calls [`begin_turn`](CompositeDriver::begin_turn) to get its own
/// [`TurnStack`].
pub struct CompositeDriver {
    middleware: Vec<Arc<dyn Middleware>>,
}

impl CompositeDriver {
    /// A driver over `middleware`, consulted in this order.
    pub fn new(middleware: Vec<Arc<dyn Middleware>>) -> Self {
        debug!(count = middleware.len(), "creating composite driver");
        Self { middleware }
    }

    /// True when no middleware is configured.
    pub fn is_empty(&self) -> bool {
        self.middleware.is_empty()
    }

    /// The number of configured middleware.
    pub fn len(&self) -> usize {
        self.middleware.len()
    }

    /// The state of one run: one instance per middleware.
    pub fn begin_turn(&self, info: &TurnInfo<'_>) -> TurnStack {
        TurnStack {
            instances: self
                .middleware
                .iter()
                .map(|middleware| Instance {
                    name: middleware.name(),
                    inner: middleware.begin_turn(info),
                })
                .collect(),
        }
    }

    /// The first manual budget pricing any middleware configures.
    pub fn budget_pricing(&self) -> Option<querymt::providers::ModelPricing> {
        self.middleware
            .iter()
            .find_map(|middleware| middleware.budget_pricing())
    }

    /// The limits of all middleware; the first value of each limit wins.
    pub fn limits(&self) -> Option<SessionLimits> {
        self.middleware
            .iter()
            .filter_map(|middleware| middleware.limits())
            .reduce(|first, next| SessionLimits {
                max_steps: first.max_steps.or(next.max_steps),
                max_turns: first.max_turns.or(next.max_turns),
                max_cost_usd: first.max_cost_usd.or(next.max_cost_usd),
            })
    }
}

struct Instance {
    name: &'static str,
    inner: Box<dyn TurnMiddleware>,
}

/// Outcome of consulting the middleware of a stage.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Folded<V> {
    /// The verdict of the first decisive middleware, or the pass verdict when
    /// every middleware passed.
    Decided(V),
    /// The run was cancelled while the stage was being consulted.
    Cancelled,
}

/// A verdict that can say "nothing to decide".
trait Verdict {
    const PASS: Self;
    fn passes(&self) -> bool;
}

impl Verdict for StepVerdict {
    const PASS: Self = StepVerdict::Proceed;
    fn passes(&self) -> bool {
        matches!(self, StepVerdict::Proceed)
    }
}

impl Verdict for ReplyVerdict {
    const PASS: Self = ReplyVerdict::Proceed;
    fn passes(&self) -> bool {
        matches!(self, ReplyVerdict::Proceed)
    }
}

impl Verdict for EndVerdict {
    const PASS: Self = EndVerdict::Finish;
    fn passes(&self) -> bool {
        matches!(self, EndVerdict::Finish)
    }
}

impl Verdict for StopVerdict {
    const PASS: Self = StopVerdict::Pass;
    fn passes(&self) -> bool {
        matches!(self, StopVerdict::Pass)
    }
}

/// The error of a hook, with the name of the middleware that returned it.
#[derive(Debug, thiserror::Error)]
#[error("middleware {name} failed: {source}")]
pub(crate) struct HookFailure {
    /// The configured name of the middleware.
    pub(crate) name: &'static str,
    #[source]
    pub(crate) source: MiddlewareError,
}

/// For the hook rig of the tests, whose steps return the bare
/// [`MiddlewareError`]. It forgets the middleware's name, so the run does not
/// use it: it converts to `TurnError::Middleware`, which keeps the name.
#[cfg(any(test, feature = "test-support"))]
impl From<HookFailure> for MiddlewareError {
    fn from(failure: HookFailure) -> Self {
        failure.source
    }
}

/// Wait for `hook`, or for cancellation, whichever comes first. A hook that
/// loses the race is dropped and its verdict discarded.
async fn race<V>(
    cancel: &CancellationToken,
    name: &'static str,
    hook: impl Future<Output = Result<V>>,
) -> std::result::Result<Option<V>, HookFailure> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Ok(None),
        verdict = hook => verdict.map(Some).map_err(|source| HookFailure { name, source }),
    }
}

/// The middleware of one run. Dropped with the run, so no state outlives it.
#[derive(Default)]
pub struct TurnStack {
    instances: Vec<Instance>,
}

/// Consult `$stack`'s instances in order with `$call`, stopping at the first
/// decisive verdict. Notices and overrides written to `$cx` before that stay.
macro_rules! consult {
    ($self:ident, $cx:ident, $cancel:ident, $phase:literal, |$mw:ident| $call:expr) => {{
        for instance in &mut $self.instances {
            if $cancel.is_cancelled() {
                return Ok(Folded::Cancelled);
            }
            let span = debug_span!("middleware.driver", name = instance.name, phase = $phase);
            let $mw = &mut instance.inner;
            match race($cancel, instance.name, $call.instrument(span)).await? {
                None => return Ok(Folded::Cancelled),
                Some(verdict) if !Verdict::passes(&verdict) => return Ok(Folded::Decided(verdict)),
                Some(_) => {}
            }
        }
        Ok(Folded::Decided(Verdict::PASS))
    }};
}

impl TurnStack {
    /// True when no middleware is configured.
    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// Before the first step of the run.
    pub(crate) async fn turn_start(
        &mut self,
        cx: &mut HookCx<'_>,
        cancel: &CancellationToken,
    ) -> std::result::Result<Folded<StepVerdict>, HookFailure> {
        consult!(self, cx, cancel, "turn_start", |mw| mw.on_turn_start(cx))
    }

    /// Before a model request.
    pub(crate) async fn step_start(
        &mut self,
        cx: &mut HookCx<'_>,
        cancel: &CancellationToken,
    ) -> std::result::Result<Folded<StepVerdict>, HookFailure> {
        consult!(self, cx, cancel, "step_start", |mw| mw.on_step_start(cx))
    }

    /// After a model reply.
    pub(crate) async fn after_llm(
        &mut self,
        cx: &mut HookCx<'_>,
        reply: &LlmResponse,
        cancel: &CancellationToken,
    ) -> std::result::Result<Folded<ReplyVerdict>, HookFailure> {
        consult!(self, cx, cancel, "after_llm", |mw| mw
            .on_after_llm(cx, reply))
    }

    /// When the model has ended its turn.
    pub(crate) async fn turn_end(
        &mut self,
        cx: &mut HookCx<'_>,
        cancel: &CancellationToken,
    ) -> std::result::Result<Folded<EndVerdict>, HookFailure> {
        consult!(self, cx, cancel, "turn_end", |mw| mw.on_turn_end(cx))
    }

    /// When the run is about to end on `stop`. The first middleware that
    /// recovers ends the stage. When none does, what they queued while they
    /// were consulted is dropped: it was meant for a run that goes on, and the
    /// stop goes on to publication unchanged.
    pub(crate) async fn stopped(
        &mut self,
        cx: &mut HookCx<'_>,
        stop: &Stop,
        cancel: &CancellationToken,
    ) -> std::result::Result<Folded<StopVerdict>, HookFailure> {
        let checkpoint = cx.checkpoint();
        let folded = self.consult_stopped(cx, stop, cancel).await?;
        if matches!(folded, Folded::Decided(StopVerdict::Pass)) {
            cx.rollback(checkpoint);
        }
        Ok(folded)
    }

    async fn consult_stopped(
        &mut self,
        cx: &mut HookCx<'_>,
        stop: &Stop,
        cancel: &CancellationToken,
    ) -> std::result::Result<Folded<StopVerdict>, HookFailure> {
        consult!(self, cx, cancel, "stopped", |mw| mw.on_stopped(cx, stop))
    }

    /// Reviews the prepared calls of one scheduling segment. A call rejected
    /// by one middleware is not shown to later ones. Returns one verdict per
    /// input call, in order.
    pub(crate) async fn prepared_tool_calls(
        &mut self,
        cx: &mut HookCx<'_>,
        batch: &PreparedBatch<'_>,
    ) -> std::result::Result<Vec<PreparedCallVerdict>, HookFailure> {
        let mut verdicts: Vec<PreparedCallVerdict> = batch
            .calls
            .iter()
            .map(|_| PreparedCallVerdict::Proceed)
            .collect();
        for instance in &mut self.instances {
            let live: Vec<usize> = verdicts
                .iter()
                .enumerate()
                .filter(|(_, verdict)| matches!(verdict, PreparedCallVerdict::Proceed))
                .map(|(index, _)| index)
                .collect();
            if live.is_empty() {
                break;
            }
            let subset: Vec<PreparedToolCall<'_>> =
                live.iter().map(|&index| batch.calls[index]).collect();
            let reviewed = instance
                .inner
                .on_prepared_tool_calls(
                    cx,
                    &PreparedBatch {
                        session_id: batch.session_id,
                        cwd: batch.cwd,
                        calls: &subset,
                    },
                )
                .instrument(debug_span!(
                    "middleware.driver",
                    name = instance.name,
                    phase = "prepared_tool_calls"
                ))
                .await
                .map_err(|source| HookFailure {
                    name: instance.name,
                    source,
                })?;
            for (&index, verdict) in live.iter().zip(reviewed) {
                verdicts[index] = verdict;
            }
        }
        Ok(verdicts)
    }
}
