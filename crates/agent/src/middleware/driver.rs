use crate::agent::core::SessionRuntime;
use crate::events::SessionLimits;
use crate::middleware::{ExecutionState, Result};
use async_trait::async_trait;
use log::{debug, trace};
use std::sync::Arc;
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
        self.run_phase(state, runtime, MiddlewarePhase::TurnStart)
            .await
    }

    pub async fn run_step_start(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        self.run_phase(state, runtime, MiddlewarePhase::StepStart)
            .await
    }

    pub async fn run_after_llm(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        self.run_phase(state, runtime, MiddlewarePhase::AfterLlm)
            .await
    }

    pub async fn run_processing_tool_calls(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        self.run_phase(state, runtime, MiddlewarePhase::ProcessingToolCalls)
            .await
    }

    pub async fn run_turn_end(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        self.run_phase(state, runtime, MiddlewarePhase::TurnEnd)
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

            let span = info_span!("middleware.driver", name = %driver_name, phase = %phase.name());
            current = match phase {
                MiddlewarePhase::TurnStart => {
                    driver
                        .on_turn_start(current, runtime)
                        .instrument(span)
                        .await?
                }
                MiddlewarePhase::StepStart => {
                    driver
                        .on_step_start(current, runtime)
                        .instrument(span)
                        .await?
                }
                MiddlewarePhase::AfterLlm => {
                    driver
                        .on_after_llm(current, runtime)
                        .instrument(span)
                        .await?
                }
                MiddlewarePhase::ProcessingToolCalls => {
                    driver
                        .on_processing_tool_calls(current, runtime)
                        .instrument(span)
                        .await?
                }
                MiddlewarePhase::TurnEnd => {
                    driver
                        .on_turn_end(current, runtime)
                        .instrument(span)
                        .await?
                }
            };

            let new_state_name = current.name();
            trace!(
                "  Driver {} transitioned: {} -> {}",
                driver_name, current_state_name, new_state_name
            );

            // If state became terminal, stop processing further middleware
            if matches!(
                current,
                ExecutionState::Complete
                    | ExecutionState::Stopped { .. }
                    | ExecutionState::Cancelled
            ) {
                debug!(
                    "CompositeDriver: {} produced terminal state {}, stopping pipeline",
                    driver_name, new_state_name
                );
                break;
            }
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
}

impl MiddlewarePhase {
    fn name(&self) -> &'static str {
        match self {
            MiddlewarePhase::TurnStart => "turn_start",
            MiddlewarePhase::StepStart => "step_start",
            MiddlewarePhase::AfterLlm => "after_llm",
            MiddlewarePhase::ProcessingToolCalls => "processing_tool_calls",
            MiddlewarePhase::TurnEnd => "turn_end",
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
            stats: Arc::new(AgentStats::default()),
            provider: "mock".into(),
            model: "mock-model".into(),
            session_mode: crate::agent::core::AgentMode::Build,
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
            stats: Arc::new(AgentStats::default()),
            provider: "mock".into(),
            model: "mock-model".into(),
            session_mode: crate::agent::core::AgentMode::Build,
        });

        let state = ExecutionState::BeforeLlmCall { context };
        let result = composite.run_turn_start(state, None).await.unwrap();

        // Should stop at the second driver and not run third
        assert!(matches!(result, ExecutionState::Stopped { .. }));
    }
}
