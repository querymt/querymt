use crate::events::SessionLimits;
use crate::middleware::{ExecutionState, Result};
use async_trait::async_trait;
use log::{debug, trace};
use std::sync::Arc;
use tracing::{Instrument, info_span, instrument};

/// Trait for middleware that drives state transitions
#[async_trait]
pub trait MiddlewareDriver: Send + Sync {
    /// Transform the current execution state to the next state
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState>;

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
}

#[async_trait]
impl MiddlewareDriver for CompositeDriver {
    #[instrument(
        name = "middleware.pipeline",
        skip(self, state),
        fields(
            input_state = %state.name(),
            output_state = tracing::field::Empty,
            drivers_count = %self.drivers.len()
        )
    )]
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        let state_name = state.name();
        trace!("CompositeDriver::next_state entering state: {}", state_name);

        let mut current = state;

        for (idx, driver) in self.drivers.iter().enumerate() {
            let driver_name = driver.name();
            let current_state_name = current.name();

            trace!(
                "  Running driver {}/{}: {} on state: {}",
                idx + 1,
                self.drivers.len(),
                driver_name,
                current_state_name
            );

            current = driver
                .next_state(current)
                .instrument(info_span!("middleware.driver", name = %driver_name))
                .await?;

            let new_state_name = current.name();
            trace!(
                "  Driver {} transitioned: {} → {}",
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
            "CompositeDriver::next_state exiting state: {}",
            final_state_name
        );

        // Record the output state in the tracing span
        tracing::Span::current().record("output_state", final_state_name);

        Ok(current)
    }

    fn reset(&self) {
        debug!(
            "Resetting CompositeDriver with {} middleware",
            self.drivers.len()
        );
        for driver in &self.drivers {
            trace!("Resetting driver: {}", driver.name());
            driver.reset();
        }
    }

    fn name(&self) -> &'static str {
        "CompositeDriver"
    }

    fn get_limits(&self) -> Option<SessionLimits> {
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
        async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
            if self.should_stop {
                Ok(ExecutionState::Stopped {
                    message: "test stop".into(),
                    stop_type: StopType::Other,
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
        });

        let state = ExecutionState::BeforeTurn { context };
        let result = composite.next_state(state).await.unwrap();

        assert!(matches!(result, ExecutionState::BeforeTurn { .. }));
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
        });

        let state = ExecutionState::BeforeTurn { context };
        let result = composite.next_state(state).await.unwrap();

        // Should stop at the second driver and not run third
        assert!(matches!(result, ExecutionState::Stopped { .. }));
    }
}
