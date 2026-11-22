//! Agent mode middleware - injects mode-specific reminders based on the current agent mode
//!
//! This middleware reads the agent's current mode (Build, Plan, Review) and queues
//! a stored `agent_mode` notice when the mode's reminder is not already the most
//! recent one in history. In Build mode (default), no reminder is stored.
//!
//! # Example (programmatic)
//!
//! ```ignore
//! use querymt_agent::middleware::AgentModeMiddleware;
//!
//! let agent = Agent::builder()
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
//!
//! [middleware.reminders]
//! plan = "You are in plan mode. Only observe, analyze, and plan."
//! review = "You are in review mode. Review code and provide feedback."
//! ```

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

use super::{HookCx, Middleware, Result, StepVerdict, TurnInfo, TurnMiddleware};
use crate::agent::core::AgentMode;
use crate::middleware::factory::MiddlewareFactory;
use querymt_agent_core::model::NoticeSource;
use serde::Deserialize;

/// Middleware that injects mode-specific reminder messages based on the current agent mode.
///
/// Stores a map of `AgentMode` → reminder string. When the agent is in a mode that
/// has a reminder, it is stored once as a notice when the mode changes.
/// Build mode typically has no reminder (full read/write).
///
/// The mode is the session's, read through `HookCx::mode`.
#[derive(Clone)]
pub struct AgentModeMiddleware {
    reminders: Arc<HashMap<AgentMode, String>>,
}

impl AgentModeMiddleware {
    /// Create with a single plan-mode reminder (convenience constructor).
    pub fn new(plan_reminder: String) -> Self {
        let mut reminders = HashMap::new();
        reminders.insert(AgentMode::Plan, plan_reminder);
        Self::with_reminders(reminders)
    }

    /// Create with explicit per-mode reminders.
    pub fn with_reminders(reminders: HashMap<AgentMode, String>) -> Self {
        Self {
            reminders: Arc::new(reminders),
        }
    }
}

/// The notice to store before the next request, if any.
///
/// Compares the session mode with the most recent `agent_mode` notice in
/// the effective history (stored or queued): a reminder is stored when the
/// mode has one and the last notice is missing or another mode's; a leave
/// notice is stored when the mode has none and the last notice is a
/// reminder. Any stored mode notice other than the leave notice counts as a
/// reminder, so one written under an earlier reminder text is still left.
/// Checked per request, not per switch, so switches without a request store
/// nothing and a reminder lost to compaction is stored again.
fn notice_for(reminders: &HashMap<AgentMode, String>, cx: &HookCx<'_>) -> Option<String> {
    let last = cx.last_mode_notice();
    match reminders.get(&cx.mode()) {
        Some(reminder) => (last != Some(reminder.as_str())).then(|| reminder.clone()),
        None => last
            .filter(|last| *last != LEAVE_NOTICE)
            .map(|_| LEAVE_NOTICE.to_string()),
    }
}

/// Stored when the session leaves a mode that had a reminder.
const LEAVE_NOTICE: &str =
    "The agent mode changed. The instructions from the earlier mode reminder no longer apply.";

impl Middleware for AgentModeMiddleware {
    fn name(&self) -> &'static str {
        "AgentModeMiddleware"
    }

    fn begin_turn(&self, _turn: &TurnInfo<'_>) -> Box<dyn TurnMiddleware> {
        Box::new(AgentModeTurn {
            reminders: Arc::clone(&self.reminders),
        })
    }
}

/// The mode check of one run. It keeps no state of its own: what the last
/// notice was is read from the history.
struct AgentModeTurn {
    reminders: Arc<HashMap<AgentMode, String>>,
}

#[async_trait]
impl TurnMiddleware for AgentModeTurn {
    async fn on_step_start(&mut self, cx: &mut HookCx<'_>) -> Result<StepVerdict> {
        if let Some(content) = notice_for(&self.reminders, cx) {
            cx.queue_notice(NoticeSource::AgentMode, content);
        }
        Ok(StepVerdict::Proceed)
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
/// reminder = "Plan mode message"
/// review_reminder = "Review mode message"
/// ```
///
/// Advanced with explicit reminders map:
/// ```toml
/// [[middleware]]
/// type = "agent_mode"
/// [middleware.reminders]
/// plan = "Plan mode message"
/// review = "Review mode message"
/// ```
///
/// The initial session mode is not configured here; it is the typed `mode`
/// key on `[agent]`, `[planner]` and `[[delegates]]`. Unknown keys (including
/// the former `default`) are rejected.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentModeFactoryConfig {
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
        _agent_config: &crate::agent::agent_config::AgentConfig,
    ) -> anyhow::Result<Option<Arc<dyn Middleware>>> {
        let cfg: AgentModeFactoryConfig = serde_json::from_value(config.clone())?;

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

        // Use per-session mode path (reads from ConversationContext.session_mode)
        Ok(Some(Arc::new(AgentModeMiddleware::with_reminders(
            reminders,
        ))))
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
        _agent_config: &crate::agent::agent_config::AgentConfig,
    ) -> anyhow::Result<Option<Arc<dyn Middleware>>> {
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

        // Note: initial mode is now set via AgentConfig.default_mode / session actor.
        // The legacy `enabled` flag maps to the default mode being Plan vs Build,
        // but we no longer set a global AtomicU8 here.
        let _ = cfg.enabled; // Acknowledged but handled at agent build level

        let mut reminders = HashMap::new();
        reminders.insert(AgentMode::Plan, cfg.reminder);
        reminders.insert(
            AgentMode::Review,
            cfg.review_reminder.unwrap_or_else(default_review_reminder),
        );

        // Use per-session mode path (reads from ConversationContext.session_mode)
        Ok(Some(Arc::new(AgentModeMiddleware::with_reminders(
            reminders,
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::PendingNotice;
    use crate::middleware::factory::MIDDLEWARE_REGISTRY;
    use crate::test_utils::HookRig;
    use querymt_agent_core::model::{AgentMessage, MessagePart};

    fn middleware() -> AgentModeMiddleware {
        let mut reminders = HashMap::new();
        reminders.insert(AgentMode::Plan, "plan reminder".to_string());
        reminders.insert(AgentMode::Review, "review reminder".to_string());
        AgentModeMiddleware::with_reminders(reminders)
    }

    /// What a request starts from: the session's mode and the mode notice
    /// the stored history holds.
    #[derive(Clone)]
    struct Context {
        mode: AgentMode,
        stored: Option<String>,
    }

    impl Context {
        fn last_mode_notice(&self) -> Option<&str> {
            self.stored.as_deref()
        }
    }

    fn context(mode: AgentMode, stored: Option<&str>) -> Context {
        Context {
            mode,
            stored: stored.map(str::to_string),
        }
    }

    /// What the middleware handed on for one request.
    struct Stepped {
        pending_notices: Vec<PendingNotice>,
        last_mode_notice: Option<String>,
    }

    /// Run the middleware for one request.
    async fn step(middleware: &AgentModeMiddleware, context: Context) -> Stepped {
        let mut stack = HookRig::stack_of(middleware.clone());
        let mut rig = HookRig::new();
        rig.mode = context.mode;
        if let Some(content) = &context.stored {
            let mut message = AgentMessage::new("s".into(), querymt::chat::ChatRole::User);
            message.parts.push(MessagePart::Notice {
                source: NoticeSource::AgentMode,
                content: content.clone(),
            });
            rig.set_history(vec![message]);
        }
        let _ = rig.step_start(&mut stack).await.unwrap();
        Stepped {
            last_mode_notice: rig.cx().last_mode_notice().map(str::to_string),
            pending_notices: rig.pending,
        }
    }

    fn queued(stepped: &Stepped) -> Vec<(NoticeSource, &str)> {
        stepped
            .pending_notices
            .iter()
            .map(|notice| (notice.source, notice.content.as_str()))
            .collect()
    }

    /// Mimics the loop storing queued notices: the last one becomes the stored notice.
    fn stored(stepped: &Stepped, mode: AgentMode) -> Context {
        self::context(mode, stepped.last_mode_notice.as_deref())
    }

    #[tokio::test]
    async fn reminder_is_stored_once_for_a_plan_mode_run() {
        let m = middleware();
        let mut ctx = context(AgentMode::Plan, None);
        let mut stored_count = 0;
        for _ in 0..5 {
            let next = step(&m, ctx.clone()).await;
            stored_count += next.pending_notices.len();
            if !next.pending_notices.is_empty() {
                assert_eq!(
                    queued(&next),
                    vec![(NoticeSource::AgentMode, "plan reminder")]
                );
            }
            ctx = stored(&next, AgentMode::Plan);
        }
        assert_eq!(stored_count, 1);
    }

    #[tokio::test]
    async fn switch_from_plan_to_review_stores_the_review_reminder_without_a_leave_notice() {
        let m = middleware();
        let next = step(&m, context(AgentMode::Review, Some("plan reminder"))).await;
        assert_eq!(
            queued(&next),
            vec![(NoticeSource::AgentMode, "review reminder")]
        );
    }

    #[tokio::test]
    async fn switching_without_a_request_stores_nothing() {
        // Build -> plan -> review -> build with no request in between: the
        // middleware only runs at a request, and the session is in build mode.
        let m = middleware();
        let next = step(&m, context(AgentMode::Build, None)).await;
        assert!(next.pending_notices.is_empty());
    }

    #[tokio::test]
    async fn reminder_is_restored_after_compaction() {
        // Compaction removed the stored reminder: no stored mode notice remains.
        let m = middleware();
        let next = step(&m, context(AgentMode::Plan, None)).await;
        assert_eq!(
            queued(&next),
            vec![(NoticeSource::AgentMode, "plan reminder")]
        );
    }

    #[tokio::test]
    async fn build_session_stores_no_mode_notice() {
        let m = middleware();
        let mut ctx = context(AgentMode::Build, None);
        for _ in 0..3 {
            let next = step(&m, ctx.clone()).await;
            assert!(next.pending_notices.is_empty());
            ctx = stored(&next, AgentMode::Build);
        }
        assert_eq!(ctx.last_mode_notice(), None);
    }

    #[tokio::test]
    async fn returning_to_plan_mode_stores_the_reminder_again() {
        let m = middleware();
        let left = step(&m, context(AgentMode::Build, Some("plan reminder"))).await;
        let leave = queued(&left)[0].1.to_string();
        let back = step(&m, stored(&left, AgentMode::Plan)).await;
        assert_eq!(
            queued(&back),
            vec![(NoticeSource::AgentMode, "plan reminder")]
        );
        assert_ne!(leave, "plan reminder");
    }

    #[tokio::test]
    async fn leaving_plan_mode_stores_a_leave_notice_after_the_earlier_reminder() {
        let m = middleware();
        let next = step(&m, context(AgentMode::Build, Some("plan reminder"))).await;
        let notices = queued(&next);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0, NoticeSource::AgentMode);
        assert_eq!(notices[0].1, LEAVE_NOTICE);
    }

    #[tokio::test]
    async fn leaving_a_mode_whose_reminder_text_changed_stores_a_leave_notice() {
        // The stored reminder was written under an earlier configuration.
        let m = middleware();
        let next = step(&m, context(AgentMode::Build, Some("old plan reminder"))).await;
        assert_eq!(queued(&next), vec![(NoticeSource::AgentMode, LEAVE_NOTICE)]);
    }

    #[tokio::test]
    async fn leave_notice_is_stored_once() {
        let m = middleware();
        let mut ctx = context(AgentMode::Build, Some("plan reminder"));
        let mut count = 0;
        for _ in 0..4 {
            let next = step(&m, ctx.clone()).await;
            count += next.pending_notices.len();
            ctx = stored(&next, AgentMode::Build);
        }
        assert_eq!(count, 1);
    }

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
        assert!(cfg.reminder.is_none());
        assert!(cfg.review_reminder.is_none());
        assert!(cfg.reminders.is_none());
    }

    #[test]
    fn test_agent_mode_config_simple_format() {
        let config = serde_json::json!({
            "reminder": "Custom plan reminder",
            "review_reminder": "Custom review reminder"
        });
        let cfg: AgentModeFactoryConfig = serde_json::from_value(config).unwrap();
        assert_eq!(cfg.reminder.unwrap(), "Custom plan reminder");
        assert_eq!(cfg.review_reminder.unwrap(), "Custom review reminder");
    }

    #[test]
    fn test_agent_mode_config_advanced_format() {
        let config = serde_json::json!({
            "reminders": {
                "plan": "Plan msg",
                "review": "Review msg"
            }
        });
        let cfg: AgentModeFactoryConfig = serde_json::from_value(config).unwrap();
        let reminders = cfg.reminders.unwrap();
        assert_eq!(reminders.get("plan").unwrap(), "Plan msg");
        assert_eq!(reminders.get("review").unwrap(), "Review msg");
    }

    #[test]
    fn agent_mode_config_rejects_the_former_default_key() {
        let err = serde_json::from_value::<AgentModeFactoryConfig>(
            serde_json::json!({"default": "plan"}),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("default"), "{err}");
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
