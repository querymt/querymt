//! Where a run is in its loop, and the one function that moves it.
//!
//! A [`Stage`] is a position that owns the data its step consumes. It is moved
//! into [`Stage::advance`], which returns the next stage or the run's outcome,
//! so a run can only end through a [`CycleOutcome`]. The state of the run
//! itself (history, statistics, queued notices, budgets) lives in
//! [`Turn`]; [`TurnEnv`] holds what never changes during the run.

use super::turn::{self, Turn, TurnEnv};
use super::{
    CycleOutcome, DrainedSteering, SteeringBoundary, apply_pending_steering, drain_steering,
    emit_stop_hook_cap_notice, format_stop_hook_continuation_message,
    has_effective_task_completion_tool, maintenance, prepare_model_request, queue_progress_review,
    run_stop_hook, transitions, wait, warn_incomplete_budget,
};
use crate::acp::protocol::StopReason;
use crate::agent::agent_config::AgentConfig;
use crate::agent::execution::TurnError;
use crate::agent::turn_control::RunPhase;
use crate::agent::utils::u32_from_usize;
use crate::middleware::{
    EndVerdict, Folded, LlmResponse, PreparedModelRequest, ReplyVerdict, StepVerdict, Stop,
    StopVerdict, ToolCall, ToolResult, TurnInfo, TurnStack, WaitCondition,
};
use crate::session::provider::{ResolvedModel, SessionHandle};
use log::{debug, info, warn};
use querymt_agent_core::events::{AgentEventKind, EventEnvelope, ExecutionMetrics, StopType};
use std::ops::ControlFlow;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug_span, info_span, instrument};

/// A position in the loop, with the data its step consumes.
///
/// A cancelled run is not a position: it is [`CycleOutcome::Cancelled`].
#[derive(Debug, strum::IntoStaticStr)]
pub(crate) enum Stage {
    /// Reconcile, drain steering, run step middleware and prepare a request.
    BeforeRequest,
    /// Send the prepared request.
    CallModel(Box<PreparedModelRequest>),
    /// Review the model's reply with middleware, then store it.
    AfterReply(Box<LlmResponse>),
    /// Run the calls of a reply; `results` already holds the answered ones.
    ToolCalls {
        calls: Vec<ToolCall>,
        results: Vec<ToolResult>,
    },
    /// Wait for delegations to report back.
    Waiting(WaitCondition),
    /// The reply ended the turn: steering, end-of-run middleware, Stop hook,
    /// completion guard, pruning.
    Ending,
    /// Middleware stopped the run: compaction, recovery, Stop hook, publication.
    Stopping(Stop),
}

// The stage is moved on every iteration; boxing keeps the large variants off it.
const _: () = assert!(size_of::<Stage>() <= 64);

/// What a step returns: the next stage, or the end of the run.
pub(crate) type Step = ControlFlow<CycleOutcome, Stage>;

/// The step that ends the run as cancelled.
pub(crate) fn cancelled() -> Step {
    ControlFlow::Break(CycleOutcome::Cancelled)
}

/// The step that moves to `Stopping` with a stop of `stop_type`.
pub(crate) fn stopping(stop_type: StopType, message: impl Into<std::sync::Arc<str>>) -> Step {
    ControlFlow::Continue(Stage::Stopping(Stop::new(stop_type, message)))
}

/// The step after the middleware of a hook that precedes a model request.
pub(crate) fn after_step(folded: Folded<StepVerdict>) -> Step {
    match folded {
        Folded::Cancelled => cancelled(),
        Folded::Decided(StepVerdict::Proceed) => ControlFlow::Continue(Stage::BeforeRequest),
        Folded::Decided(StepVerdict::Stop(stop)) => ControlFlow::Continue(Stage::Stopping(stop)),
    }
}

/// What a run holds besides its environment and its state: the middleware of
/// the run and the agent's event stream (read while waiting for delegations).
/// The model the run talks to is not here: it is resolved before the run's
/// environment exists and lives in [`TurnEnv::model`].
pub(crate) struct Run<'a> {
    config: &'a AgentConfig,
    stack: TurnStack,
    events: broadcast::Receiver<std::sync::Arc<EventEnvelope>>,
}

/// Run the loop to its outcome.
///
/// This is the entry point of the session actor's execution task: the run's
/// model is already resolved (`resolve_model`) and is part of `env`.
/// `session_mode` is the `AgentMode` captured at turn start; middleware reads
/// it through its hook context.
///
/// The turn future is built and boxed here, so callers hold a pointer instead
/// of the turn's whole state (large in unoptimized builds, where it would add
/// to every caller's stack frame).
pub(crate) fn run_turn<'a>(
    config: &'a AgentConfig,
    env: &'a TurnEnv,
    turn: &'a mut Turn,
    session_mode: crate::agent::core::AgentMode,
) -> futures_util::future::BoxFuture<'a, Result<CycleOutcome, TurnError>> {
    Box::pin(run_turn_unboxed(config, env, turn, session_mode))
}

#[instrument(
    name = "agent.execution.turn",
    skip(config, env, turn),
    fields(
        session_id = %env.session_id,
        mode = %session_mode,
        llm_request_count = tracing::field::Empty,
        completion_guard_available = tracing::field::Empty,
        completion_guard_continuations = tracing::field::Empty,
    )
)]
async fn run_turn_unboxed(
    config: &AgentConfig,
    env: &TurnEnv,
    turn: &mut Turn,
    session_mode: crate::agent::core::AgentMode,
) -> Result<CycleOutcome, TurnError> {
    debug!("Starting run for session: {}", env.session_id);

    let stack = info_span!("agent.execution.driver_create").in_scope(|| {
        config
            .create_driver()
            .begin_turn(&TurnInfo::new(&env.session_id, session_mode))
    });
    let events = config.event_sink.fanout().subscribe();
    turn.stats = crate::middleware::AgentStats {
        turns: turn.history.user_turns(),
        ..Default::default()
    };
    let mut run = Run {
        config,
        stack,
        events,
    };

    let mut stage = match run.first(env, turn).await? {
        ControlFlow::Continue(stage) => stage,
        ControlFlow::Break(outcome) => return Ok(outcome),
    };
    loop {
        stage = match stage.advance(&mut run, env, turn).await? {
            ControlFlow::Continue(next) => next,
            ControlFlow::Break(outcome) => return Ok(outcome),
        };
    }
}

/// Resolve the model a run talks to: its provider and the facts about it,
/// under the `provider_init` retry, cancellable while it waits. This is where
/// the cold-start cost of a provider lands, and the one place a run resolves
/// a model: the result is a field of [`TurnEnv`] and every step reads it.
///
/// It takes the session handle and not a [`TurnEnv`], because the environment
/// needs the model to exist. `Ok(None)` means the run was cancelled while the
/// provider was being built; any other failure is a provider-initialization
/// error.
pub(crate) async fn resolve_model(
    config: &AgentConfig,
    session_handle: &SessionHandle,
    cancel: &CancellationToken,
) -> Result<Option<ResolvedModel>, TurnError> {
    let session_id = &session_handle.session().public_id;
    let llm_config = session_handle
        .llm_config()
        .ok_or_else(|| anyhow::anyhow!("No LLM config for session"))?;
    debug!(
        "Session {}: llm_config provider={} model={}",
        session_id, llm_config.provider, llm_config.model
    );

    let resolved =
        super::llm_retry::call_with_retry(config, session_id, "provider_init", cancel, || {
            let session_handle = session_handle.clone();
            let cancel = cancel.clone();
            async move {
                tokio::select! {
                    result = session_handle.resolve_model() => {
                        result.map_err(querymt::error::LLMError::from)
                    }
                    _ = cancel.cancelled() => Err(querymt::error::LLMError::Cancelled),
                }
            }
        })
        .instrument(info_span!("agent.execution.provider_init"))
        .await;
    match resolved {
        Ok(model) => Ok(Some(model)),
        Err(querymt::error::LLMError::Cancelled) => Ok(None),
        Err(error) => Err(transitions::provider_error(
            error,
            "provider initialization",
            &llm_config.provider,
            &llm_config.model,
        )),
    }
}

/// [`resolve_model`] without a retry, for native `/compact`: a failure to build
/// the provider is the provider-initialization error at once. It has no
/// cancellation token to race either, as the command runs to completion. The
/// caller decides when the failure ends the command (see
/// `compact_history_with`): a `pre_compaction` hook may still supply the
/// summary.
pub(crate) async fn resolve_model_once(
    session_handle: &SessionHandle,
) -> Result<ResolvedModel, TurnError> {
    let llm_config = session_handle
        .llm_config()
        .ok_or_else(|| anyhow::anyhow!("No LLM config for session"))?;
    session_handle.resolve_model().await.map_err(|error| {
        transitions::provider_error(
            querymt::error::LLMError::from(error),
            "provider initialization",
            &llm_config.provider,
            &llm_config.model,
        )
    })
}

impl Stage {
    /// Move one position on. Cancellation is checked first, except in
    /// `ToolCalls`, which must store one result per pending call.
    pub(crate) async fn advance(
        self,
        run: &mut Run<'_>,
        env: &TurnEnv,
        turn: &mut Turn,
    ) -> Result<Step, TurnError> {
        if env.cancellation_token.is_cancelled() && !matches!(self, Stage::ToolCalls { .. }) {
            info!("Session {}: cancelled at loop top", env.session_id);
            return Ok(cancelled());
        }
        debug!(
            "Run stage {} for session {}",
            <&'static str>::from(&self),
            env.session_id
        );
        match self {
            Stage::BeforeRequest => run.before_request(env, turn).await,
            Stage::CallModel(request) => run.call_model(&request, env, turn).await,
            Stage::AfterReply(reply) => run.after_reply(reply, env, turn).await,
            Stage::ToolCalls { calls, results } => {
                run.tool_calls(&calls, &results, env, turn).await
            }
            Stage::Waiting(wait) => {
                env.report_phase(RunPhase::Waiting);
                wait::transition_waiting_for_event(run.config, &wait, env, turn, &mut run.events)
                    .await
            }
            Stage::Ending => run.ending(env, turn).await,
            Stage::Stopping(stop) => run.stopping(stop, env, turn).await,
        }
    }
}

impl Run<'_> {
    /// The first middleware hook.
    async fn first(&mut self, env: &TurnEnv, turn: &mut Turn) -> Result<Step, TurnError> {
        // The run's history was loaded when the run was set up (`History::load`)
        // and holds the prompt of this run.
        debug!(
            "Session {}: run starts with {} messages, cancelled={}",
            env.session_id,
            turn.history.as_slice().len(),
            env.cancellation_token.is_cancelled()
        );
        let started = {
            let mut cx = turn.hook_cx(env);
            self.stack
                .turn_start(&mut cx, &env.cancellation_token)
                .instrument(debug_span!(
                    "agent.execution.middleware.turn_start",
                    session_id = %env.session_id
                ))
                .await?
        };
        debug!(
            "Session {}: turn_start done, cancelled={}",
            env.session_id,
            env.cancellation_token.is_cancelled()
        );
        Ok(after_step(started))
    }

    async fn before_request(&mut self, env: &TurnEnv, turn: &mut Turn) -> Result<Step, TurnError> {
        let config = self.config;
        // Everything from here to request dispatch is agent overhead, not model time.
        let step_started = std::time::Instant::now();
        env.report_phase(RunPhase::Model);
        if !env.restricted_mesh_turn() {
            wait::reconcile_suspended_delegations(config, env, turn, &mut self.events).await?;
        }
        apply_pending_steering(config, env, turn, SteeringBoundary::NextRequest, false).await?;
        let steps = turn.stats.steps;
        let checkpoint_due = crate::agent::objective::progress_review_due(
            steps,
            turn.budgets.last_objective_checkpoint_step(),
        );
        if checkpoint_due {
            turn.budgets.set_last_objective_checkpoint_step(steps);
            // Stored as a notice whether or not the objective is sent.
            queue_progress_review(turn);
            config.emit_event(
                &env.session_id,
                AgentEventKind::ObjectiveCheckpoint {
                    run_id: env.turn_id().to_string(),
                    reason: "periodic_progress_review".to_string(),
                    action: "review_progress".to_string(),
                    objective_revision: turn.run_objective.as_ref().map(|o| o.revision),
                    current_task_id: turn.state.active_task.as_ref().map(|t| t.public_id.clone()),
                    current_task_revision: turn.state.active_task.as_ref().map(|t| t.revision),
                },
            );
        }
        // Price limits read this cycle's committed accounting, which
        // includes cache and auxiliary (compaction/summary) attempts
        // that settled since the last main response.
        let totals = env.cycle_accounting.cycle_totals();
        warn_incomplete_budget(config, env, turn, &totals);
        turn.stats.apply_recorded_costs(&totals);
        let stepped = {
            let mut cx = turn.hook_cx(env);
            self.stack
                .step_start(&mut cx, &env.cancellation_token)
                .instrument(debug_span!(
                    "agent.execution.middleware.step_start",
                    session_id = %env.session_id
                ))
                .await?
        };
        match stepped {
            Folded::Cancelled => Ok(cancelled()),
            Folded::Decided(StepVerdict::Stop(stop)) => {
                Ok(ControlFlow::Continue(Stage::Stopping(stop)))
            }
            Folded::Decided(StepVerdict::Proceed) => {
                // Middleware queued its notices during step start; store
                // them now, after any tool results and before the request.
                let next = prepare_model_request(config, env, turn).await?;
                crate::agent::utils::llm_metrics::step_overhead(
                    "pre_request",
                    step_started.elapsed().as_secs_f64(),
                    crate::agent::utils::genai::provider_name(env.provider_name()),
                    env.model_name(),
                );
                Ok(next)
            }
        }
    }

    async fn call_model(
        &mut self,
        request: &PreparedModelRequest,
        env: &TurnEnv,
        turn: &mut Turn,
    ) -> Result<Step, TurnError> {
        // The tools the model can call, not the request's tool list:
        // a tool change in the messages adds to or removes from that.
        turn.budgets
            .note_llm_request(has_effective_task_completion_tool(&request.offered_tools));
        let span = tracing::Span::current();
        span.record("llm_request_count", u64::from(turn.budgets.llm_requests()));
        span.record(
            "completion_guard_available",
            turn.budgets.task_completion_tool_available(),
        );
        transitions::transition_call_llm(self.config, request, env, turn).await
    }

    async fn after_reply(
        &mut self,
        reply: Box<LlmResponse>,
        env: &TurnEnv,
        turn: &mut Turn,
    ) -> Result<Step, TurnError> {
        let reviewed = {
            let mut cx = turn.hook_cx(env);
            self.stack
                .after_llm(&mut cx, &reply, &env.cancellation_token)
                .instrument(debug_span!(
                    "agent.execution.middleware.after_llm",
                    session_id = %env.session_id
                ))
                .await?
        };
        match reviewed {
            Folded::Cancelled => Ok(cancelled()),
            Folded::Decided(ReplyVerdict::Proceed) => {
                transitions::transition_after_llm(self.config, &reply, env, turn).await
            }
            Folded::Decided(ReplyVerdict::Stop(stop)) => {
                Ok(ControlFlow::Continue(Stage::Stopping(stop)))
            }
            Folded::Decided(ReplyVerdict::Retry) => {
                // The discarded reply was billed: its usage counts.
                crate::middleware::apply_usage_to_stats(&mut turn.stats, reply.usage.as_ref());
                Ok(ControlFlow::Continue(Stage::BeforeRequest))
            }
        }
    }

    async fn tool_calls(
        &mut self,
        calls: &[ToolCall],
        results: &[ToolResult],
        env: &TurnEnv,
        turn: &mut Turn,
    ) -> Result<Step, TurnError> {
        env.report_phase(RunPhase::Tools);
        let steering = if results.is_empty() {
            drain_steering(env, SteeringBoundary::ToolBatch, false).await
        } else {
            DrainedSteering::none()
        };
        if steering.is_empty() {
            return transitions::transition_processing_tool_calls(
                self.config,
                &mut self.stack,
                calls,
                results,
                env,
                turn,
                steering,
            )
            .await;
        }
        // Steering arrived before the batch started: every call is skipped,
        // and the skipped results are stored directly after the assistant
        // message, before the steering.
        let skipped: Vec<ToolResult> = calls
            .iter()
            .map(|call| {
                ToolResult::new(
                    call.id.clone(),
                    vec![querymt::chat::ToolResultPart::Text {
                        text: "Skipped because new user steering was received".to_string(),
                    }],
                    true,
                    Some(call.function.name.clone()),
                    Some(call.function.arguments.clone()),
                )
            })
            .collect();
        transitions::transition_processing_tool_calls(
            self.config,
            &mut self.stack,
            &[],
            &skipped,
            env,
            turn,
            steering,
        )
        .await
    }

    /// The reply ended the turn.
    async fn ending(&mut self, env: &TurnEnv, turn: &mut Turn) -> Result<Step, TurnError> {
        let config = self.config;
        let again = || Ok(ControlFlow::Continue(Stage::BeforeRequest));
        env.report_phase(RunPhase::Closing);
        turn.last_applied = querymt::chat::GenerationOptions::NONE;
        if apply_pending_steering(config, env, turn, SteeringBoundary::RunEnd, true).await? {
            return again();
        }
        let ended = {
            let mut cx = turn.hook_cx(env);
            self.stack
                .turn_end(&mut cx, &env.cancellation_token)
                .instrument(debug_span!(
                    "agent.execution.middleware.turn_end",
                    session_id = %env.session_id
                ))
                .await?
        };
        match ended {
            Folded::Cancelled => return Ok(cancelled()),
            Folded::Decided(EndVerdict::Continue) => return again(),
            Folded::Decided(EndVerdict::Stop(stop)) => {
                info!(
                    "Turn-end middleware stopped: {} ({:?})",
                    stop.message, stop.stop_type
                );
                return Ok(ControlFlow::Continue(Stage::Stopping(stop)));
            }
            Folded::Decided(EndVerdict::Finish) => {}
        }
        debug!("Run reached its end");

        let stop_hook = run_stop_hook(config, env, turn::StopHookKind::EndTurn).await?;
        if stop_hook.should_continue {
            let max_stop_hook_continuations = config.execution_policy.max_stop_hook_continuations;
            if turn
                .budgets
                .stop_hook_continuations(turn::StopHookKind::EndTurn)
                >= max_stop_hook_continuations
            {
                emit_stop_hook_cap_notice(
                    config,
                    env,
                    turn::StopHookKind::EndTurn,
                    max_stop_hook_continuations,
                );
            } else {
                turn.budgets
                    .note_stop_hook_continuation(turn::StopHookKind::EndTurn);
                let message = format_stop_hook_continuation_message(
                    stop_hook.stop_reason.or(stop_hook.block_reason),
                    &stop_hook.additional_contexts,
                );
                turn.queue_notice(querymt_agent_core::model::NoticeSource::StopHook, message);
                return again();
            }
        }

        if config.execution_policy.task_completion_guard
            && turn.budgets.task_completion_tool_available()
            && let Some(task) = turn.state.active_task.as_ref()
            && task.status == crate::session::domain::TaskStatus::Active
            && task.kind == crate::session::domain::TaskKind::Finite
        {
            if turn.budgets.completion_guard_continuations() == 0 {
                turn.budgets.note_completion_guard_continuation();
                tracing::Span::current().record(
                    "completion_guard_continuations",
                    u64::from(turn.budgets.completion_guard_continuations()),
                );
                config.emit_event(
                    &env.session_id,
                    AgentEventKind::TaskCompletionGuardTriggered {
                        run_id: env.turn_id().to_string(),
                        task_id: task.public_id.clone(),
                    },
                );
                turn.queue_notice(
                    querymt_agent_core::model::NoticeSource::TaskCompletion,
                    "<system-reminder>\nThe current finite task remains active. Continue the work and call complete_task with concrete evidence when it is done, or ask the user for required information. An ordinary response stop does not complete the task.\n\nThis message was generated by the QueryMT task-completion runtime, not by the user.\n</system-reminder>".to_string(),
                );
                return again();
            }
            config.emit_event(
                &env.session_id,
                AgentEventKind::TaskCompletionGuardExhausted {
                    run_id: env.turn_id().to_string(),
                    task_id: task.public_id.clone(),
                },
            );
        }

        if !env.restricted_mesh_turn()
            && config.execution_policy.pruning.enabled
            && let Err(e) = maintenance::run_pruning(config, env).await
        {
            warn!("Pruning failed: {}", e);
        }

        Ok(ControlFlow::Break(CycleOutcome::Completed))
    }

    /// Middleware stopped the run.
    async fn stopping(
        &mut self,
        stop: Stop,
        env: &TurnEnv,
        turn: &mut Turn,
    ) -> Result<Step, TurnError> {
        let config = self.config;
        let again = || Ok(ControlFlow::Continue(Stage::BeforeRequest));
        let Stop { stop_type, message } = &stop;
        let stop_type = *stop_type;
        info!("Run stopped: {} ({:?})", message, stop_type);

        if stop_type == StopType::ContextThreshold
            && !env.restricted_mesh_turn()
            && config.execution_policy.compaction.auto
        {
            turn.budgets.mark_automatic_compaction()?;
            info!("Context threshold reached, triggering AI compaction");

            match maintenance::run_ai_compaction(config, env, turn).await {
                Ok(next) => return Ok(ControlFlow::Continue(next)),
                Err(e) => warn!("AI compaction failed: {}", e),
            }
        }

        // Give middleware the chance to replace the stop with a
        // continuation. The per-turn guard bounds any recovery loop.
        let max_recoveries = config.execution_policy.max_stop_recoveries;
        if turn.budgets.stop_recoveries() >= max_recoveries {
            warn!(
                "Session {}: stop recovery limit ({}) reached; publishing stop",
                env.session_id, max_recoveries
            );
        } else {
            let recovered = {
                let mut cx = turn.hook_cx(env);
                self.stack
                    .stopped(&mut cx, &stop, &env.cancellation_token)
                    .instrument(info_span!(
                        "agent.execution.middleware.stopped",
                        session_id = %env.session_id
                    ))
                    .await?
            };
            match recovered {
                Folded::Cancelled => return Ok(cancelled()),
                Folded::Decided(StopVerdict::Recover) => {
                    turn.budgets.note_stop_recovery();
                    debug!(
                        "Session {}: stop recovered by middleware ({}/{})",
                        env.session_id,
                        turn.budgets.stop_recoveries(),
                        max_recoveries
                    );
                    return again();
                }
                // Nobody recovered: the stop goes on to publication
                // unchanged (the stack dropped what was queued).
                Folded::Decided(StopVerdict::Pass) => {}
            }
        }

        // A token-limit stop that middleware could not recover still
        // gives Stop hooks a say, so unattended loops can carry on.
        if stop_type == StopType::ModelTokenLimit {
            let stop_hook = run_stop_hook(config, env, turn::StopHookKind::ModelTokenLimit).await?;
            if stop_hook.should_continue {
                let max = config.execution_policy.max_stop_hook_continuations;
                if turn
                    .budgets
                    .stop_hook_continuations(turn::StopHookKind::ModelTokenLimit)
                    >= max
                {
                    emit_stop_hook_cap_notice(
                        config,
                        env,
                        turn::StopHookKind::ModelTokenLimit,
                        max,
                    );
                } else {
                    turn.budgets
                        .note_stop_hook_continuation(turn::StopHookKind::ModelTokenLimit);
                    let message = format_stop_hook_continuation_message(
                        stop_hook.stop_reason.or(stop_hook.block_reason),
                        &stop_hook.additional_contexts,
                    );
                    turn.queue_notice(querymt_agent_core::model::NoticeSource::StopHook, message);
                    return again();
                }
            }
        }

        let metrics = ExecutionMetrics {
            steps: u32_from_usize(turn.stats.steps, "stats.steps", Some(&env.session_id)),
            turns: u32_from_usize(turn.stats.turns, "stats.turns", Some(&env.session_id)),
        };

        config.emit_event(
            &env.session_id,
            AgentEventKind::MiddlewareStopped {
                stop_type,
                reason: message.to_string(),
                metrics,
            },
        );

        Ok(ControlFlow::Break(CycleOutcome::Stopped {
            stop_reason: StopReason::from(stop_type),
            stop_type,
            reason: message.to_string(),
        }))
    }
}
