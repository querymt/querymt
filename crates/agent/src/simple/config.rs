//! Agent configuration types for the simple builder API

use super::utils::infer_required_capabilities;
use crate::config::MiddlewareEntry;
use crate::tools::CapabilityRequirement;
use querymt::LLMParams;
use serde_json::Value;

/// Internal configuration for building agents
#[derive(Clone)]
pub(super) struct AgentConfig {
    pub id: String,
    pub llm_config: Option<LLMParams>,
    pub capabilities: Vec<String>,
    pub tools: Vec<String>,
    pub description: Option<String>,
    pub required_capabilities: Vec<CapabilityRequirement>,
    pub middleware: Vec<MiddlewareEntry>,
}

impl AgentConfig {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            llm_config: None,
            capabilities: Vec::new(),
            tools: Vec::new(),
            description: None,
            required_capabilities: Vec::new(),
            middleware: Vec::new(),
        }
    }
}

/// Builder for configuring a planner agent
pub struct PlannerConfigBuilder {
    pub(super) config: AgentConfig,
}

impl PlannerConfigBuilder {
    pub(super) fn new() -> Self {
        Self {
            config: AgentConfig::new("planner"),
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

    pub fn system(self, value: impl Into<String>) -> Self {
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

    pub fn reasoning(self, value: bool) -> Self {
        self.with_llm(|c| c.reasoning(value))
    }

    pub fn reasoning_effort(self, value: impl Into<String>) -> Self {
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

    pub fn tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.config.tools = tools.into_iter().map(Into::into).collect();
        self
    }

    pub(super) fn build(self) -> AgentConfig {
        self.config
    }
}

/// Builder for configuring a delegate agent
pub struct DelegateConfigBuilder {
    pub(super) config: AgentConfig,
}

impl DelegateConfigBuilder {
    pub(super) fn new(id: impl Into<String>) -> Self {
        Self {
            config: AgentConfig::new(id),
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

    pub fn system(self, value: impl Into<String>) -> Self {
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

    pub fn reasoning(self, value: bool) -> Self {
        self.with_llm(|c| c.reasoning(value))
    }

    pub fn reasoning_effort(self, value: impl Into<String>) -> Self {
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
        S: Into<String>,
    {
        self.config.tools = tools.into_iter().map(Into::into).collect();
        self
    }

    pub(super) fn build(mut self) -> AgentConfig {
        // Auto-infer required_capabilities from tools
        self.config.required_capabilities = infer_required_capabilities(&self.config.tools)
            .into_iter()
            .collect();
        self.config
    }
}

#[cfg(test)]
mod tests {
    use super::super::utils::build_llm_config;
    use super::*;

    #[test]
    fn test_delegate_system_prompt_stored() {
        use querymt::LLMParams;

        let mut config = AgentConfig::new("test-delegate");
        config.llm_config = Some(
            LLMParams::new()
                .provider("openai")
                .model("gpt-4")
                .system("You are a helpful assistant."),
        );

        let result = build_llm_config(&config).unwrap();
        assert_eq!(
            result.system.as_deref(),
            Some("You are a helpful assistant.")
        );
    }
}
