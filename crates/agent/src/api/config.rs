//! Delegate configuration for the simple builder API

use crate::agent::core::AgentMode;
use crate::config::{Permissions, SkillsConfig};
use crate::delegation::member::{MemberBuildSpec, infer_required_capabilities, parse_tool_specs};
use crate::delegation::roster::AgentId;
use crate::worktree::DelegationIsolation;
use querymt::{LLMParams, chat::ReasoningEffort};
use serde_json::Value;

/// Builder for configuring a delegate agent.
///
/// A malformed tool selector is remembered and reported by
/// [`DelegateConfigBuilder::build`], which the agent builder calls.
pub struct DelegateConfigBuilder {
    pub(super) config: MemberBuildSpec,
    /// The first problem found while configuring.
    error: Option<anyhow::Error>,
}

impl DelegateConfigBuilder {
    pub(super) fn new(id: AgentId) -> Self {
        Self {
            config: MemberBuildSpec::new(id),
            error: None,
        }
    }

    // Helper that lazily initializes LLMParams
    fn with_llm<F>(mut self, f: F) -> Self
    where
        F: FnOnce(LLMParams) -> LLMParams,
    {
        let cfg = self.config.llm_config.take().unwrap_or_default();
        self.config.llm_config = Some(f(cfg));
        self
    }

    pub fn provider(self, name: impl Into<String>, model: impl Into<String>) -> Self {
        self.with_llm(|c| c.provider(name).model(model))
    }

    pub fn api_key(self, key: impl Into<String>) -> Self {
        self.with_llm(|c| c.api_key(key))
    }

    pub fn system(self, value: impl Into<querymt::params::SystemPrompt>) -> Self {
        self.with_llm(|c| c.system(value))
    }

    pub fn temperature(self, value: f32) -> Self {
        self.with_llm(|c| c.temperature(value))
    }

    pub fn max_tokens(self, value: u32) -> Self {
        self.with_llm(|c| c.max_tokens(value))
    }

    pub fn top_p(self, value: f32) -> Self {
        self.with_llm(|c| c.top_p(value))
    }

    pub fn top_k(self, value: u32) -> Self {
        self.with_llm(|c| c.top_k(value))
    }

    pub fn reasoning_effort(self, value: ReasoningEffort) -> Self {
        self.with_llm(|c| c.reasoning_effort(value))
    }

    pub fn timeout_seconds(self, value: u64) -> Self {
        self.with_llm(|c| c.timeout_seconds(value))
    }

    pub fn base_url(self, value: impl Into<String>) -> Self {
        self.with_llm(|c| c.base_url(value))
    }

    pub fn parameter<K: Into<String>>(self, key: K, value: impl Into<Value>) -> Self {
        self.with_llm(|c| c.parameter(key, value.into()))
    }

    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.config.description = Some(desc.into());
        self
    }

    pub fn capabilities<I, S>(mut self, caps: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.config.capabilities = caps.into_iter().map(Into::into).collect();
        self
    }

    pub fn tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let (specs, error) = parse_tool_specs(tools);
        self.config.tools = specs;
        if let Some(error) = error {
            self.error.get_or_insert_with(|| {
                anyhow::anyhow!(
                    "delegate '{}': invalid `tools` selector: {error}",
                    self.config.id
                )
            });
        }
        self
    }

    /// Initial mode of new sessions for this agent.
    pub fn mode(mut self, mode: AgentMode) -> Self {
        self.config.mode = mode;
        self
    }

    pub fn skills(mut self, config: SkillsConfig) -> Self {
        self.config.skills = Some(config);
        self
    }

    /// Workspace isolation for tasks delegated to this delegate. An explicit
    /// non-`Inherit` value on the `delegate` tool call still wins.
    pub fn isolation(mut self, isolation: DelegationIsolation) -> Self {
        self.config.isolation = isolation;
        self
    }

    /// Tool approval policy for this delegate.
    pub fn permissions(mut self, permissions: Permissions) -> Self {
        self.config.permissions = permissions;
        self
    }

    pub(super) fn build(mut self) -> anyhow::Result<MemberBuildSpec> {
        if let Some(error) = self.error {
            return Err(error);
        }
        // Auto-infer required_capabilities from tools
        self.config.required_capabilities = infer_required_capabilities(&self.config.tools)
            .into_iter()
            .collect();
        Ok(self.config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_delegate_system_prompt_stored() {
        let mut config = MemberBuildSpec::new(AgentId::try_from("test-delegate").unwrap());
        config.llm_config = Some(
            LLMParams::new()
                .provider("openai")
                .model("gpt-4")
                .system("You are a helpful assistant."),
        );

        let result = config.llm_params().unwrap();
        assert_eq!(
            result.system,
            vec!["You are a helpful assistant.".to_string()]
        );
    }
}
