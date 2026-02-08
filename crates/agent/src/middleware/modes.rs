//! Agent mode middleware - injects mode-specific reminders based on the current agent mode
//!
//! This middleware reads the agent's current mode (Build, Plan, Review) and injects
//! an appropriate reminder message before each LLM call. In Build mode (default),
//! no reminder is injected.
//!
//! # Example (programmatic)
//!
//! ```ignore
//! use querymt_agent::middleware::AgentModeMiddleware;
//!
//! let agent = Agent::single()
//!     .provider("anthropic", "claude-sonnet")
//!     .with_agent_mode(AgentMode::Plan)
//!     .with_agent_mode_middleware("You are in plan mode. Only observe and plan.")
//!     .build()
//!     .await?;
//! ```
//!
//! # Example (TOML config)
//!
//! ```toml
//! [[middleware]]
//! type = "agent_mode"
//! default = "build"
//!
//! [middleware.reminders]
//! plan = "You are in plan mode. Only observe, analyze, and plan."
//! review = "You are in review mode. Review code and provide feedback."
//! ```

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use super::{ExecutionState, MiddlewareDriver, Result};
use crate::agent::core::AgentMode;
use crate::middleware::factory::MiddlewareFactory;
use log::trace;
use serde::Deserialize;

/// Middleware that injects mode-specific reminder messages based on the current agent mode.
///
/// Stores a map of `AgentMode` → reminder string. When the agent is in a mode that
/// has a reminder, it is injected as a user message before the LLM call.
/// Build mode typically has no reminder (full read/write).
pub struct AgentModeMiddleware {
    agent_mode: Arc<AtomicU8>,
    reminders: HashMap<AgentMode, String>,
}

impl AgentModeMiddleware {
    /// Create with a single plan-mode reminder (convenience constructor).
    pub fn new(agent_mode: Arc<AtomicU8>, plan_reminder: String) -> Self {
        let mut reminders = HashMap::new();
        reminders.insert(AgentMode::Plan, plan_reminder);
        Self {
            agent_mode,
            reminders,
        }
    }

    /// Create with explicit per-mode reminders.
    pub fn with_reminders(
        agent_mode: Arc<AtomicU8>,
        reminders: HashMap<AgentMode, String>,
    ) -> Self {
        Self {
            agent_mode,
            reminders,
        }
    }
}

#[async_trait]
impl MiddlewareDriver for AgentModeMiddleware {
    async fn on_turn_start(&self, state: ExecutionState) -> Result<ExecutionState> {
        match state {
            ExecutionState::BeforeLlmCall { ref context } => {
                let mode = AgentMode::from_u8(self.agent_mode.load(Ordering::Relaxed));

                if let Some(reminder) = self.reminders.get(&mode) {
                    trace!(
                        "AgentModeMiddleware: injecting reminder for {:?} mode",
                        mode
                    );
                    let new_context = context.inject_message(reminder.clone());
                    Ok(ExecutionState::BeforeLlmCall {
                        context: Arc::new(new_context),
                    })
                } else {
                    Ok(state)
                }
            }
            other => Ok(other),
        }
    }

    fn reset(&self) {
        // No state to reset
    }

    fn name(&self) -> &'static str {
        "AgentModeMiddleware"
    }
}

// ============================================================================
// Factory for config-based creation
// ============================================================================

/// Config for agent_mode middleware from TOML.
///
/// Supports two formats:
///
/// Simple (backward compatible with plan_mode):
/// ```toml
/// [[middleware]]
/// type = "agent_mode"
/// default = "build"
/// reminder = "Plan mode message"
/// review_reminder = "Review mode message"
/// ```
///
/// Advanced with explicit reminders map:
/// ```toml
/// [[middleware]]
/// type = "agent_mode"
/// default = "build"
/// [middleware.reminders]
/// plan = "Plan mode message"
/// review = "Review mode message"
/// ```
#[derive(Debug, Deserialize)]
struct AgentModeFactoryConfig {
    /// Initial mode (defaults to "build")
    #[serde(default = "default_mode")]
    default: String,
    /// Plan mode reminder (simple format, backward compat)
    #[serde(default)]
    reminder: Option<String>,
    /// Review mode reminder (simple format)
    #[serde(default)]
    review_reminder: Option<String>,
    /// Explicit reminders map (advanced format, takes precedence)
    #[serde(default)]
    reminders: Option<HashMap<String, String>>,
}

fn default_mode() -> String {
    "build".to_string()
}

fn default_plan_reminder() -> String {
    "You are in plan mode. Only observe, analyze, and plan. Do not make changes.".to_string()
}

fn default_review_reminder() -> String {
    "You are in review mode. Review the code carefully and provide constructive feedback. \
     Focus on code quality, potential bugs, performance issues, and adherence to best practices. \
     Do not make changes yourself."
        .to_string()
}

/// Factory for creating AgentModeMiddleware from config
pub struct AgentModeFactory;

impl MiddlewareFactory for AgentModeFactory {
    fn type_name(&self) -> &'static str {
        "agent_mode"
    }

    fn create(
        &self,
        config: &serde_json::Value,
        agent: &crate::agent::core::QueryMTAgent,
    ) -> anyhow::Result<Arc<dyn MiddlewareDriver>> {
        let cfg: AgentModeFactoryConfig = serde_json::from_value(config.clone())?;

        // Set the initial agent mode
        let initial_mode: AgentMode = cfg
            .default
            .parse()
            .map_err(|e: String| anyhow::anyhow!(e))?;
        agent.set_agent_mode(initial_mode);

        // Build reminders map
        let mut reminders = HashMap::new();

        if let Some(ref map) = cfg.reminders {
            // Advanced format: explicit reminders map
            for (mode_str, reminder) in map {
                let mode: AgentMode = mode_str.parse().map_err(|e: String| anyhow::anyhow!(e))?;
                reminders.insert(mode, reminder.clone());
            }
        } else {
            // Simple format: use reminder / review_reminder fields
            let plan = cfg.reminder.unwrap_or_else(default_plan_reminder);
            reminders.insert(AgentMode::Plan, plan);

            if let Some(review) = cfg.review_reminder {
                reminders.insert(AgentMode::Review, review);
            } else {
                reminders.insert(AgentMode::Review, default_review_reminder());
            }
        }

        Ok(Arc::new(AgentModeMiddleware::with_reminders(
            agent.agent_mode_flag(),
            reminders,
        )))
    }
}

/// Backward-compatible factory that registers as "plan_mode" type name.
/// Delegates to AgentModeFactory internally.
pub struct PlanModeCompatFactory;

impl MiddlewareFactory for PlanModeCompatFactory {
    fn type_name(&self) -> &'static str {
        "plan_mode"
    }

    fn create(
        &self,
        config: &serde_json::Value,
        agent: &crate::agent::core::QueryMTAgent,
    ) -> anyhow::Result<Arc<dyn MiddlewareDriver>> {
        // Parse the old plan_mode config format
        #[derive(Debug, Deserialize)]
        struct LegacyConfig {
            #[serde(default = "legacy_default_enabled")]
            enabled: bool,
            #[serde(default = "default_plan_reminder")]
            reminder: String,
            #[serde(default)]
            review_reminder: Option<String>,
        }

        fn legacy_default_enabled() -> bool {
            true
        }

        let cfg: LegacyConfig = serde_json::from_value(config.clone())?;

        // Set initial mode based on legacy enabled flag
        if cfg.enabled {
            agent.set_agent_mode(AgentMode::Plan);
        } else {
            agent.set_agent_mode(AgentMode::Build);
        }

        let mut reminders = HashMap::new();
        reminders.insert(AgentMode::Plan, cfg.reminder);
        reminders.insert(
            AgentMode::Review,
            cfg.review_reminder.unwrap_or_else(default_review_reminder),
        );

        Ok(Arc::new(AgentModeMiddleware::with_reminders(
            agent.agent_mode_flag(),
            reminders,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::factory::MIDDLEWARE_REGISTRY;

    #[test]
    fn test_agent_mode_factory_registered() {
        let types = MIDDLEWARE_REGISTRY.type_names();
        assert!(types.contains(&"agent_mode"));
    }

    #[test]
    fn test_plan_mode_compat_factory_registered() {
        let types = MIDDLEWARE_REGISTRY.type_names();
        assert!(types.contains(&"plan_mode"));
    }

    #[test]
    fn test_agent_mode_config_defaults() {
        let config = serde_json::json!({});
        let cfg: AgentModeFactoryConfig = serde_json::from_value(config).unwrap();
        assert_eq!(cfg.default, "build");
        assert!(cfg.reminder.is_none());
        assert!(cfg.review_reminder.is_none());
        assert!(cfg.reminders.is_none());
    }

    #[test]
    fn test_agent_mode_config_simple_format() {
        let config = serde_json::json!({
            "default": "plan",
            "reminder": "Custom plan reminder",
            "review_reminder": "Custom review reminder"
        });
        let cfg: AgentModeFactoryConfig = serde_json::from_value(config).unwrap();
        assert_eq!(cfg.default, "plan");
        assert_eq!(cfg.reminder.unwrap(), "Custom plan reminder");
        assert_eq!(cfg.review_reminder.unwrap(), "Custom review reminder");
    }

    #[test]
    fn test_agent_mode_config_advanced_format() {
        let config = serde_json::json!({
            "default": "build",
            "reminders": {
                "plan": "Plan msg",
                "review": "Review msg"
            }
        });
        let cfg: AgentModeFactoryConfig = serde_json::from_value(config).unwrap();
        assert_eq!(cfg.default, "build");
        let reminders = cfg.reminders.unwrap();
        assert_eq!(reminders.get("plan").unwrap(), "Plan msg");
        assert_eq!(reminders.get("review").unwrap(), "Review msg");
    }

    #[test]
    fn test_agent_mode_factory_type_name() {
        let factory = AgentModeFactory;
        assert_eq!(factory.type_name(), "agent_mode");
    }

    #[test]
    fn test_plan_mode_compat_factory_type_name() {
        let factory = PlanModeCompatFactory;
        assert_eq!(factory.type_name(), "plan_mode");
    }
}
