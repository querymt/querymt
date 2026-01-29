//! Plan mode middleware - restricts agent to read-only observation and planning
//!
//! This middleware injects a reminder message when plan mode is enabled, instructing
//! the agent to only observe, analyze, and plan without making changes.
//!
//! # Example (programmatic)
//!
//! ```ignore
//! use querymt_agent::middleware::PlanModeMiddleware;
//!
//! let agent = Agent::single()
//!     .provider("anthropic", "claude-sonnet")
//!     .with_plan_mode_enabled(true)
//!     .with_plan_mode_middleware("You are in plan mode. Only observe and plan.")
//!     .build()
//!     .await?;
//! ```
//!
//! # Example (TOML config)
//!
//! ```toml
//! [[middleware]]
//! type = "plan_mode"
//! enabled = true
//! reminder = "You are in plan mode. Only observe, analyze, and plan. Do not make changes."
//! ```
//!
//! Or use defaults:
//!
//! ```toml
//! [[middleware]]
//! type = "plan_mode"
//! # enabled defaults to true
//! # reminder has a sensible default message
//! ```

use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{ExecutionState, MiddlewareDriver, Result};
use crate::middleware::factory::MiddlewareFactory;
use log::trace;
use serde::Deserialize;

/// Middleware that injects a reminder message when plan mode is enabled
pub struct PlanModeMiddleware {
    enabled: Arc<AtomicBool>,
    reminder: String,
}

impl PlanModeMiddleware {
    pub fn new(enabled: Arc<AtomicBool>, reminder: String) -> Self {
        Self { enabled, reminder }
    }
}

#[async_trait]
impl MiddlewareDriver for PlanModeMiddleware {
    async fn on_turn_start(&self, state: ExecutionState) -> Result<ExecutionState> {
        match state {
            ExecutionState::BeforeLlmCall { ref context }
                if self.enabled.load(Ordering::Relaxed) =>
            {
                trace!("PlanModeMiddleware: injecting reminder message");
                let new_context = context.inject_message(self.reminder.clone());
                Ok(ExecutionState::BeforeLlmCall {
                    context: Arc::new(new_context),
                })
            }
            other => Ok(other),
        }
    }

    fn reset(&self) {
        // No state to reset
    }

    fn name(&self) -> &'static str {
        "PlanModeMiddleware"
    }
}

// ============================================================================
// Factory for config-based creation
// ============================================================================

/// Config for plan_mode middleware from TOML
#[derive(Debug, Deserialize)]
struct PlanModeFactoryConfig {
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default = "default_plan_mode_reminder")]
    reminder: String,
}

fn default_enabled() -> bool {
    true
}

fn default_plan_mode_reminder() -> String {
    "You are in plan mode. Only observe, analyze, and plan. Do not make changes.".to_string()
}

/// Factory for creating PlanModeMiddleware from config
pub struct PlanModeFactory;

impl MiddlewareFactory for PlanModeFactory {
    fn type_name(&self) -> &'static str {
        "plan_mode"
    }

    fn create(
        &self,
        config: &serde_json::Value,
        agent: &crate::agent::core::QueryMTAgent,
    ) -> anyhow::Result<Arc<dyn MiddlewareDriver>> {
        let cfg: PlanModeFactoryConfig = serde_json::from_value(config.clone())?;

        // Set the agent's plan_mode_enabled AtomicBool to the config value
        // This allows runtime toggling while the middleware remains registered
        agent.set_plan_mode(cfg.enabled);

        // Create middleware sharing the agent's AtomicBool
        Ok(Arc::new(PlanModeMiddleware::new(
            agent.plan_mode_flag(),
            cfg.reminder,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::factory::MIDDLEWARE_REGISTRY;

    #[test]
    fn test_plan_mode_factory_registered() {
        let types = MIDDLEWARE_REGISTRY.type_names();
        assert!(types.contains(&"plan_mode"));
    }

    #[test]
    fn test_plan_mode_config_defaults() {
        let config = serde_json::json!({});
        let cfg: PlanModeFactoryConfig = serde_json::from_value(config).unwrap();
        assert!(cfg.enabled);
        assert!(cfg.reminder.contains("plan mode"));
    }

    #[test]
    fn test_plan_mode_config_custom() {
        let config = serde_json::json!({
            "enabled": false,
            "reminder": "Custom reminder"
        });
        let cfg: PlanModeFactoryConfig = serde_json::from_value(config).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.reminder, "Custom reminder");
    }

    #[test]
    fn test_plan_mode_factory_type_name() {
        let factory = PlanModeFactory;
        assert_eq!(factory.type_name(), "plan_mode");
    }
}
