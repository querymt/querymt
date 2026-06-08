use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct HooksConfig {
    pub enabled: bool,
    pub bypass_trust: bool,
    pub session_start: Vec<MatcherGroupConfig>,
    pub user_prompt_submit: Vec<MatcherGroupConfig>,
    pub pre_tool_use: Vec<MatcherGroupConfig>,
    pub permission_request: Vec<MatcherGroupConfig>,
    pub post_tool_use: Vec<MatcherGroupConfig>,
    pub stop: Vec<MatcherGroupConfig>,
    pub pre_compaction: Vec<MatcherGroupConfig>,
    pub post_compaction: Vec<MatcherGroupConfig>,
    pub pre_delegation: Vec<MatcherGroupConfig>,
    pub delegation_start: Vec<MatcherGroupConfig>,
    pub post_delegation: Vec<MatcherGroupConfig>,
    pub delegation_failure: Vec<MatcherGroupConfig>,
    #[schemars(skip)]
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MatcherGroupConfig {
    pub matcher: Option<String>,
    pub hooks: Vec<HookHandlerConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookHandlerConfig {
    Command(HookCommandConfig),
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HookCommandConfig {
    pub command: String,
    pub timeout_sec: Option<u64>,
    pub status_message: Option<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEventConfig {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    Stop,
    PreCompaction,
    PostCompaction,
    PreDelegation,
    DelegationStart,
    PostDelegation,
    DelegationFailure,
}

impl HookEventConfig {
    pub fn label(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::PreToolUse => "pre_tool_use",
            Self::PermissionRequest => "permission_request",
            Self::PostToolUse => "post_tool_use",
            Self::Stop => "stop",
            Self::PreCompaction => "pre_compaction",
            Self::PostCompaction => "post_compaction",
            Self::PreDelegation => "pre_delegation",
            Self::DelegationStart => "delegation_start",
            Self::PostDelegation => "post_delegation",
            Self::DelegationFailure => "delegation_failure",
        }
    }
}

impl HooksConfig {
    pub fn groups_for(&self, event: HookEventConfig) -> &[MatcherGroupConfig] {
        match event {
            HookEventConfig::SessionStart => &self.session_start,
            HookEventConfig::UserPromptSubmit => &self.user_prompt_submit,
            HookEventConfig::PreToolUse => &self.pre_tool_use,
            HookEventConfig::PermissionRequest => &self.permission_request,
            HookEventConfig::PostToolUse => &self.post_tool_use,
            HookEventConfig::Stop => &self.stop,
            HookEventConfig::PreCompaction => &self.pre_compaction,
            HookEventConfig::PostCompaction => &self.post_compaction,
            HookEventConfig::PreDelegation => &self.pre_delegation,
            HookEventConfig::DelegationStart => &self.delegation_start,
            HookEventConfig::PostDelegation => &self.post_delegation,
            HookEventConfig::DelegationFailure => &self.delegation_failure,
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.extra.is_empty() {
            let mut keys: Vec<_> = self.extra.keys().cloned().collect();
            keys.sort();
            anyhow::bail!("unsupported hook event(s): {}", keys.join(", "));
        }

        for event in [
            HookEventConfig::SessionStart,
            HookEventConfig::UserPromptSubmit,
            HookEventConfig::PreToolUse,
            HookEventConfig::PermissionRequest,
            HookEventConfig::PostToolUse,
            HookEventConfig::Stop,
            HookEventConfig::PreCompaction,
            HookEventConfig::PostCompaction,
            HookEventConfig::PreDelegation,
            HookEventConfig::DelegationStart,
            HookEventConfig::PostDelegation,
            HookEventConfig::DelegationFailure,
        ] {
            for (group_idx, group) in self.groups_for(event).iter().enumerate() {
                if group.hooks.is_empty() {
                    anyhow::bail!(
                        "{} matcher group {} must contain at least one hook",
                        event.label(),
                        group_idx
                    );
                }
                for hook in &group.hooks {
                    match hook {
                        HookHandlerConfig::Command(cmd) => {
                            if cmd.command.trim().is_empty() {
                                anyhow::bail!("{} hook command must not be empty", event.label());
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
