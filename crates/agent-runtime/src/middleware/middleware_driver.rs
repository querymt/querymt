//! The `MiddlewareDriver` trait, in its own file so that edits elsewhere in the old
//! module do not move its definition (a shifted `dyn` trait span costs about a
//! minute of rebuild in the crates above).

use super::driver::*;
use crate::agent::core::SessionRuntime;
use crate::middleware::{ExecutionState, Result};
use async_trait::async_trait;
use querymt_agent_core::events::SessionLimits;
use std::sync::Arc;

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

    /// Reviews calls after argument preparation (normalisation, pre-tool hooks,
    /// validation) and before scheduling. Conflict-aware middleware must decide
    /// from `batch`'s effective arguments and accesses, never from raw model
    /// output. Returns one verdict per call in `batch.calls`, in order.
    async fn on_prepared_tool_calls(
        &self,
        batch: &PreparedBatch<'_>,
    ) -> Result<Vec<PreparedCallVerdict>> {
        Ok(batch
            .calls
            .iter()
            .map(|_| PreparedCallVerdict::Proceed)
            .collect())
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
        _events: &Arc<querymt_agent_services::event_sink::EventSink>,
    ) {
    }

    /// Reset internal state (called at start of execute_cycle)
    fn reset(&self);

    /// Returns a human-readable name for this driver
    fn name(&self) -> &'static str;

    /// Configured context size for models with no published window, if this driver provides one.
    #[cfg(feature = "remote")]
    fn fallback_context_tokens(&self) -> Option<usize> {
        None
    }

    /// Whether this driver can run without filesystem, tool, or auxiliary
    /// provider authority. Unclassified drivers are not run for Custom peers.
    fn supports_restricted_mesh(&self) -> bool {
        false
    }

    /// Returns session limits if this middleware enforces any.
    /// Default implementation returns None.
    fn get_limits(&self) -> Option<SessionLimits> {
        None
    }

    /// Manual budget pricing this middleware wants snapshotted onto every
    /// paid attempt, if any. It is valued separately from reported API spend
    /// and never replaces it. Default: none.
    fn budget_pricing(&self) -> Option<querymt::providers::ModelPricing> {
        None
    }
}
