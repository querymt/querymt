use crate::agent::core::SessionRuntime;
use crate::events::SessionLimits;
use crate::middleware::{ExecutionState, Result};
use async_trait::async_trait;
use log::{debug, trace};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, info_span, instrument};

/// Trait for middleware that runs at specific lifecycle phases
///
/// Methods now receive an optional `SessionRuntime` reference to access
/// per-session state like function_index and turn_diffs. This eliminates
/// the need for middleware to maintain their own session_runtime maps.
#[async_trait]
pub trait MiddlewareDriver: Send + Sync {
    /// Runs once at the start of a user turn
    async fn on_turn_start(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        Ok(state)
    }

    /// Runs before each LLM call (including tool-loop continuations)
    async fn on_step_start(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        Ok(state)
    }

    /// Runs after receiving the LLM response
    async fn on_after_llm(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        Ok(state)
    }

    /// Runs while processing multiple tool calls
    async fn on_processing_tool_calls(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        Ok(state)
    }

    /// Runs once when the turn is about to complete (state is Complete).
    /// Middleware can transform Complete → BeforeLlmCall to request corrections
    /// (e.g., post-turn code review for duplicate detection).
    async fn on_turn_end(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        Ok(state)
    }

    /// Runs when the run is about to end on a stop (state is `Stopped`),
    /// before the stop is published.
    ///
    /// Middleware may recover by returning `BeforeLlmCall` (a continuation),
    /// or `Cancelled`. Returning the `Stopped` state unchanged passes it on to
    /// the next driver and, finally, to publication.
    async fn on_stopped(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        Ok(state)
    }

    /// Called once the agent's final tool registry and event sink are known
    /// (when the `AgentConfig` is assembled). Tool-aware middleware should use
    /// this registry so they agree with the scheduler about what tools exist;
    /// middleware that surfaces warnings to UIs can keep the sink.
    fn bind_agent(
        &self,
        _registry: &Arc<crate::tools::ToolRegistry>,
        _events: &Arc<crate::event_sink::EventSink>,
    ) {
    }

    /// Reset internal state (called at start of execute_cycle)
    fn reset(&self);

    /// Returns a human-readable name for this driver
    fn name(&self) -> &'static str;

    /// Returns session limits if this middleware enforces any.
    /// Default implementation returns None.
    fn get_limits(&self) -> Option<SessionLimits> {
        None
    }
}

/// Composite driver that runs multiple middleware drivers in sequence
pub struct CompositeDriver {
    drivers: Vec<Arc<dyn MiddlewareDriver>>,
}

impl CompositeDriver {
    pub fn new(drivers: Vec<Arc<dyn MiddlewareDriver>>) -> Self {
        debug!("Creating CompositeDriver with {} middleware", drivers.len());
        Self { drivers }
    }

    pub fn is_empty(&self) -> bool {
        self.drivers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.drivers.len()
    }

    pub async fn run_turn_start(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        self.run_phase(state, runtime, MiddlewarePhase::TurnStart, None)
            .await
    }

    pub async fn run_turn_start_cancellable(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
        cancellation_token: &CancellationToken,
    ) -> Result<ExecutionState> {
        self.run_phase(
            state,
            runtime,
            MiddlewarePhase::TurnStart,
            Some(cancellation_token),
        )
        .await
    }

    pub async fn run_step_start(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        self.run_phase(state, runtime, MiddlewarePhase::StepStart, None)
            .await
    }

    pub async fn run_step_start_cancellable(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
        cancellation_token: &CancellationToken,
    ) -> Result<ExecutionState> {
        self.run_phase(
            state,
            runtime,
            MiddlewarePhase::StepStart,
            Some(cancellation_token),
        )
        .await
    }

    pub async fn run_after_llm(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        self.run_phase(state, runtime, MiddlewarePhase::AfterLlm, None)
            .await
    }

    pub async fn run_after_llm_cancellable(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
        cancellation_token: &CancellationToken,
    ) -> Result<ExecutionState> {
        self.run_phase(
            state,
            runtime,
            MiddlewarePhase::AfterLlm,
            Some(cancellation_token),
        )
        .await
    }

    pub async fn run_processing_tool_calls(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        self.run_phase(state, runtime, MiddlewarePhase::ProcessingToolCalls, None)
            .await
    }

    pub async fn run_processing_tool_calls_cancellable(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
        cancellation_token: &CancellationToken,
    ) -> Result<ExecutionState> {
        self.run_phase(
            state,
            runtime,
            MiddlewarePhase::ProcessingToolCalls,
            Some(cancellation_token),
        )
        .await
    }

    pub async fn run_turn_end(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        self.run_phase(state, runtime, MiddlewarePhase::TurnEnd, None)
            .await
    }

    pub async fn run_turn_end_cancellable(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
        cancellation_token: &CancellationToken,
    ) -> Result<ExecutionState> {
        self.run_phase(
            state,
            runtime,
            MiddlewarePhase::TurnEnd,
            Some(cancellation_token),
        )
        .await
    }

    /// Offers a `Stopped` state to each driver in order.
    ///
    /// Returns `BeforeLlmCall` as soon as a driver recovers (later drivers are
    /// skipped), `Cancelled` on cancellation, otherwise the unchanged stop.
    pub async fn run_stopped(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        self.run_phase(state, runtime, MiddlewarePhase::Stopped, None)
            .await
    }

    pub async fn run_stopped_cancellable(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
        cancellation_token: &CancellationToken,
    ) -> Result<ExecutionState> {
        self.run_phase(
            state,
            runtime,
            MiddlewarePhase::Stopped,
            Some(cancellation_token),
        )
        .await
    }

    pub fn reset(&self) {
        debug!(
            "Resetting CompositeDriver with {} middleware",
            self.drivers.len()
        );
        for driver in &self.drivers {
            trace!("Resetting driver: {}", driver.name());
            driver.reset();
        }
    }

    pub fn name(&self) -> &'static str {
        "CompositeDriver"
    }

    pub fn get_limits(&self) -> Option<SessionLimits> {
        // Aggregate limits from all drivers
        let mut limits = SessionLimits::default();
        let mut has_any = false;

        for driver in &self.drivers {
            if let Some(driver_limits) = driver.get_limits() {
                has_any = true;
                // Take the first non-None value for each limit
                if limits.max_steps.is_none() && driver_limits.max_steps.is_some() {
                    limits.max_steps = driver_limits.max_steps;
                }
                if limits.max_turns.is_none() && driver_limits.max_turns.is_some() {
                    limits.max_turns = driver_limits.max_turns;
                }
                if limits.max_cost_usd.is_none() && driver_limits.max_cost_usd.is_some() {
                    limits.max_cost_usd = driver_limits.max_cost_usd;
                }
            }
        }

        if has_any { Some(limits) } else { None }
    }

    #[instrument(
        name = "middleware.phase",
        skip(self, state, runtime),
        fields(
            phase = %phase.name(),
            input_state = %state.name(),
            output_state = tracing::field::Empty,
            drivers_count = %self.drivers.len()
        )
    )]
    async fn run_phase(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
        phase: MiddlewarePhase,
        cancellation_token: Option<&CancellationToken>,
    ) -> Result<ExecutionState> {
        let state_name = state.name();
        trace!(
            "CompositeDriver::run_phase entering {} with state: {}",
            phase.name(),
            state_name
        );

        let mut current = state;

        for (idx, driver) in self.drivers.iter().enumerate() {
            let driver_name = driver.name();
            let current_state_name = current.name();

            trace!(
                "  Running driver {}/{}: {} on phase {} with state: {}",
                idx + 1,
                self.drivers.len(),
                driver_name,
                phase.name(),
                current_state_name
            );

            let span = info_span!(
                "middleware.driver",
                name = %driver_name,
                phase = %phase.name(),
                index = idx,
                total = self.drivers.len(),
                input_state = %current_state_name,
                output_state = tracing::field::Empty,
                terminal = tracing::field::Empty,
            );
            if cancellation_token.is_some_and(CancellationToken::is_cancelled) {
                current = ExecutionState::Cancelled;
            } else {
                let invocation = match phase {
                    MiddlewarePhase::TurnStart => driver.on_turn_start(current, runtime),
                    MiddlewarePhase::StepStart => driver.on_step_start(current, runtime),
                    MiddlewarePhase::AfterLlm => driver.on_after_llm(current, runtime),
                    MiddlewarePhase::ProcessingToolCalls => {
                        driver.on_processing_tool_calls(current, runtime)
                    }
                    MiddlewarePhase::TurnEnd => driver.on_turn_end(current, runtime),
                    MiddlewarePhase::Stopped => driver.on_stopped(current, runtime),
                }
                .instrument(span.clone());

                current = if let Some(token) = cancellation_token {
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => ExecutionState::Cancelled,
                        result = invocation => result?,
                    }
                } else {
                    invocation.await?
                };
            }

            let new_state_name = current.name();
            span.record("output_state", new_state_name);
            trace!(
                "  Driver {} transitioned: {} -> {}",
                driver_name, current_state_name, new_state_name
            );

            // During TurnEnd, Complete is the expected pass-through state —
            // middleware may transform it to BeforeLlmCall (e.g., dedup_check
            // injecting a duplicate-code review). Only Stopped/Cancelled are
            // truly terminal during TurnEnd.
            //
            // During Stopped, the stop itself is the pass-through state: the
            // phase ends only when a driver recovers (BeforeLlmCall) or the
            // run is cancelled.
            let is_terminal = match phase {
                MiddlewarePhase::Stopped => matches!(
                    current,
                    ExecutionState::BeforeLlmCall { .. } | ExecutionState::Cancelled
                ),
                MiddlewarePhase::TurnEnd => {
                    matches!(
                        current,
                        ExecutionState::Stopped { .. } | ExecutionState::Cancelled
                    )
                }
                _ => {
                    matches!(
                        current,
                        ExecutionState::Complete { .. }
                            | ExecutionState::Stopped { .. }
                            | ExecutionState::Cancelled
                    )
                }
            };
            if is_terminal {
                span.record("terminal", true);
                debug!(
                    "CompositeDriver: {} produced terminal state {}, stopping pipeline",
                    driver_name, new_state_name
                );
                break;
            }

            span.record("terminal", false);
        }

        let final_state_name = current.name();
        trace!(
            "CompositeDriver::run_phase exiting {} with state: {}",
            phase.name(),
            final_state_name
        );

        // Record the output state in the tracing span
        tracing::Span::current().record("output_state", final_state_name);

        Ok(current)
    }
}

#[derive(Clone, Copy, Debug)]
enum MiddlewarePhase {
    TurnStart,
    StepStart,
    AfterLlm,
    ProcessingToolCalls,
    TurnEnd,
    Stopped,
}

impl MiddlewarePhase {
    fn name(&self) -> &'static str {
        match self {
            MiddlewarePhase::TurnStart => "turn_start",
            MiddlewarePhase::StepStart => "step_start",
            MiddlewarePhase::AfterLlm => "after_llm",
            MiddlewarePhase::ProcessingToolCalls => "processing_tool_calls",
            MiddlewarePhase::TurnEnd => "turn_end",
            MiddlewarePhase::Stopped => "stopped",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::StopType;
    use crate::middleware::{AgentStats, ConversationContext};
    use std::sync::Arc;

    struct TestDriver {
        name: &'static str,
        should_stop: bool,
    }

    #[async_trait]
    impl MiddlewareDriver for TestDriver {
        async fn on_turn_start(
            &self,
            state: ExecutionState,
            _runtime: Option<&Arc<SessionRuntime>>,
        ) -> Result<ExecutionState> {
            if self.should_stop {
                Ok(ExecutionState::Stopped {
                    message: "test stop".into(),
                    stop_type: StopType::Other,
                    context: None,
                })
            } else {
                Ok(state)
            }
        }

        fn reset(&self) {}

        fn name(&self) -> &'static str {
            self.name
        }
    }

    #[tokio::test]
    async fn test_composite_driver_pass_through() {
        let drivers = vec![
            Arc::new(TestDriver {
                name: "driver1",
                should_stop: false,
            }) as Arc<dyn MiddlewareDriver>,
            Arc::new(TestDriver {
                name: "driver2",
                should_stop: false,
            }) as Arc<dyn MiddlewareDriver>,
        ];

        let composite = CompositeDriver::new(drivers);
        let context = Arc::new(ConversationContext {
            session_id: "test".into(),
            messages: Arc::from([]),
            fragments: Arc::from([]),
            stats: Arc::new(AgentStats::default()),
            provider: "mock".into(),
            model: "mock-model".into(),
            session_mode: crate::agent::core::AgentMode::Build,
            request_overrides: Default::default(),
        });

        let state = ExecutionState::BeforeLlmCall { context };
        let result = composite.run_turn_start(state, None).await.unwrap();

        assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));
    }

    #[tokio::test]
    async fn test_composite_driver_stop() {
        let drivers = vec![
            Arc::new(TestDriver {
                name: "driver1",
                should_stop: false,
            }) as Arc<dyn MiddlewareDriver>,
            Arc::new(TestDriver {
                name: "stopper",
                should_stop: true,
            }) as Arc<dyn MiddlewareDriver>,
            Arc::new(TestDriver {
                name: "driver3",
                should_stop: false,
            }) as Arc<dyn MiddlewareDriver>,
        ];

        let composite = CompositeDriver::new(drivers);
        let context = Arc::new(ConversationContext {
            session_id: "test".into(),
            messages: Arc::from([]),
            fragments: Arc::from([]),
            stats: Arc::new(AgentStats::default()),
            provider: "mock".into(),
            model: "mock-model".into(),
            session_mode: crate::agent::core::AgentMode::Build,
            request_overrides: Default::default(),
        });

        let state = ExecutionState::BeforeLlmCall { context };
        let result = composite.run_turn_start(state, None).await.unwrap();

        // Should stop at the second driver and not run third
        assert!(matches!(result, ExecutionState::Stopped { .. }));
    }

    /// Records every `on_stopped` call and optionally recovers from the stop.
    struct StopRecorder {
        name: &'static str,
        recover: bool,
        calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl MiddlewareDriver for StopRecorder {
        async fn on_stopped(
            &self,
            state: ExecutionState,
            _runtime: Option<&Arc<SessionRuntime>>,
        ) -> Result<ExecutionState> {
            self.calls.lock().unwrap().push(self.name);
            match (self.recover, state.context()) {
                (true, Some(context)) => Ok(ExecutionState::BeforeLlmCall {
                    context: context.clone(),
                }),
                _ => Ok(state),
            }
        }

        fn reset(&self) {}

        fn name(&self) -> &'static str {
            self.name
        }
    }

    fn stopped_state() -> ExecutionState {
        let context = Arc::new(ConversationContext::new(
            "test".into(),
            Arc::from([]),
            Arc::new(AgentStats::default()),
            "mock".into(),
            "mock-model".into(),
        ));
        ExecutionState::Stopped {
            message: "cut off".into(),
            stop_type: StopType::ModelTokenLimit,
            context: Some(context),
        }
    }

    fn recorders(
        spec: &[(&'static str, bool)],
    ) -> (CompositeDriver, Arc<std::sync::Mutex<Vec<&'static str>>>) {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let drivers = spec
            .iter()
            .map(|&(name, recover)| {
                Arc::new(StopRecorder {
                    name,
                    recover,
                    calls: calls.clone(),
                }) as Arc<dyn MiddlewareDriver>
            })
            .collect();
        (CompositeDriver::new(drivers), calls)
    }

    #[tokio::test]
    async fn run_stopped_passes_the_stop_through_when_nobody_recovers() {
        let (composite, calls) = recorders(&[("first", false), ("second", false)]);

        let result = composite.run_stopped(stopped_state(), None).await.unwrap();

        assert!(matches!(
            result,
            ExecutionState::Stopped {
                stop_type: StopType::ModelTokenLimit,
                ..
            }
        ));
        // A Stopped result moves on, so every driver gets to act.
        assert_eq!(*calls.lock().unwrap(), vec!["first", "second"]);
    }

    #[tokio::test]
    async fn run_stopped_ends_the_phase_when_a_driver_recovers() {
        let (composite, calls) = recorders(&[("recovers", true), ("never_runs", true)]);

        let result = composite.run_stopped(stopped_state(), None).await.unwrap();

        assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));
        assert_eq!(*calls.lock().unwrap(), vec!["recovers"]);
    }

    #[tokio::test]
    async fn run_stopped_runs_drivers_in_order_until_one_recovers() {
        let (composite, calls) = recorders(&[("first", false), ("second", true), ("third", true)]);

        let result = composite.run_stopped(stopped_state(), None).await.unwrap();

        assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));
        assert_eq!(*calls.lock().unwrap(), vec!["first", "second"]);
    }

    #[tokio::test]
    async fn default_on_stopped_is_a_pass_through() {
        let composite = CompositeDriver::new(vec![Arc::new(TestDriver {
            name: "plain",
            should_stop: false,
        }) as Arc<dyn MiddlewareDriver>]);

        let result = composite.run_stopped(stopped_state(), None).await.unwrap();

        assert!(matches!(result, ExecutionState::Stopped { .. }));
    }

    #[tokio::test]
    async fn run_stopped_cancellable_reports_cancellation() {
        let (composite, calls) = recorders(&[("first", true)]);
        let token = CancellationToken::new();
        token.cancel();

        let result = composite
            .run_stopped_cancellable(stopped_state(), None, &token)
            .await
            .unwrap();

        assert!(matches!(result, ExecutionState::Cancelled));
        assert!(calls.lock().unwrap().is_empty());
    }
}
