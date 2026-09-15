//! Runtime adapters for post-construction `.agents` activation.
//!
//! Delegation target construction lives in `dotagents_targets`; this module is
//! intentionally separate because activation targets and startup triggering are
//! runtime lifecycle concerns, not delegation registry concerns.

use std::sync::{Arc, Weak};

use super::profiles::ProfileRuntimeHandle;
use crate::agent::LocalAgentHandle as AgentHandle;
use crate::dotagents::{
    DotagentsExecutionTarget, DotagentsExecutionTargetResolver, DotagentsStartupTrigger,
};

/// Resolves the default runtime and optional named profile runtimes.
pub(crate) struct SingleAgentTargetResolver {
    handle: Weak<AgentHandle>,
    profiles: Option<ProfileRuntimeHandle>,
}

impl SingleAgentTargetResolver {
    pub(crate) fn new(handle: Weak<AgentHandle>, profiles: Option<ProfileRuntimeHandle>) -> Self {
        Self { handle, profiles }
    }
}

#[async_trait::async_trait]
impl DotagentsExecutionTargetResolver for SingleAgentTargetResolver {
    async fn default_target(&self) -> Option<DotagentsExecutionTarget> {
        self.handle.upgrade()?;
        let profile_id = match &self.profiles {
            Some(profiles) => Some(profiles.active_profile_id().await),
            None => None,
        };
        Some(DotagentsExecutionTarget::new(profile_id, true))
    }

    async fn target_for_profile(&self, profile_id: &str) -> Option<DotagentsExecutionTarget> {
        self.handle.upgrade()?;
        let profiles = self.profiles.as_ref()?;
        profiles.runtime_for_profile(profile_id).await.ok()?;
        Some(DotagentsExecutionTarget::new(
            Some(profile_id.to_string()),
            true,
        ))
    }
}

/// Fires `runOnStartup` schedules through the root runtime scheduler.
pub(crate) struct SchedulerStartupTrigger {
    handle: Arc<AgentHandle>,
}

impl SchedulerStartupTrigger {
    pub(crate) fn new(handle: Arc<AgentHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait::async_trait]
impl DotagentsStartupTrigger for SchedulerStartupTrigger {
    async fn trigger_schedule(&self, schedule_public_id: &str) -> Result<(), String> {
        self.handle
            .trigger_schedule_now(schedule_public_id)
            .await
            .map_err(|error| error.to_string())
    }
}
