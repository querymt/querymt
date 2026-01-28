//! LLM configuration parameters.
//!
//! This module provides a serializable configuration struct that contains
//! only LLM parameters without operational concerns like validators or tool registries.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Parses a system prompt value (null, string, or array of strings) into `Vec<String>`.
fn parse_system_parts<E: serde::de::Error>(value: Option<Value>) -> Result<Vec<String>, E> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(vec![s]),
        Some(Value::Array(arr)) => arr
            .into_iter()
            .map(|v| match v {
                Value::String(s) => Ok(s),
                other => Err(E::custom(format!(
                    "expected string in system array, got {other}"
                ))),
            })
            .collect(),
        Some(other) => Err(E::custom(format!(
            "expected string or array for system, got {other}"
        ))),
    }
}

/// Deserializes a system prompt that can be either a single string, an array of strings,
/// or absent/null. Used by string-only providers to accept the `Vec<String>` format
/// from `LLMParams` and join them into a single string.
pub fn deserialize_system_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let parts = parse_system_parts::<D::Error>(Option::deserialize(deserializer)?)?;
    Ok(if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    })
}

/// Deserializes a system prompt that can be either a single string, an array of strings,
/// or absent/null into a `Vec<String>`. Used by providers that support multiple system
/// messages (e.g., OpenAI-compatible providers).
pub fn deserialize_system_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    parse_system_parts::<D::Error>(Option::deserialize(deserializer)?)
}

/// Pure configuration parameters for LLM providers.
///
/// This struct contains only serializable configuration data without
/// operational concerns like validators or tool registries.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct LLMParams {
    /// Optional configuration name/identifier
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Selected backend provider (e.g., "openai", "ollama", "anthropic")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,

    /// Model identifier/name to use
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// API key for authentication with the provider
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,

    /// Base URL for API requests (primarily for self-hosted instances)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    /// System prompt/context to guide model behavior.
    /// Supports multiple parts for providers that accept multi-part system prompts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<String>,

    /// Maximum tokens to generate in responses
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,

    /// Temperature parameter for controlling response randomness (0.0-1.0+)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    /// Top-p (nucleus) sampling parameter
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,

    /// Top-k sampling parameter
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,

    /// Request timeout duration in seconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,

    /// Enable reasoning mode (for providers that support it)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,

    /// Reasoning effort level (e.g., "low", "medium", "high")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,

    /// Reasoning budget in tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_budget_tokens: Option<u32>,

    /// Custom provider-specific parameters (e.g., num_ctx for Ollama)
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub custom: Option<HashMap<String, Value>>,
}

impl LLMParams {
    /// Creates a new empty params instance
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the configuration name
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the provider
    pub fn provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    /// Sets the model
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Sets the API key
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Sets the base URL
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// Appends a system prompt part. Can be called multiple times for multi-part prompts.
    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system.push(system.into());
        self
    }

    /// Sets max tokens
    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Sets temperature
    pub fn temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Sets top-p
    pub fn top_p(mut self, top_p: f32) -> Self {
        self.top_p = Some(top_p);
        self
    }

    /// Sets top-k
    pub fn top_k(mut self, top_k: u32) -> Self {
        self.top_k = Some(top_k);
        self
    }

    /// Sets timeout in seconds
    pub fn timeout_seconds(mut self, timeout_seconds: u64) -> Self {
        self.timeout_seconds = Some(timeout_seconds);
        self
    }

    /// Sets reasoning mode
    pub fn reasoning(mut self, reasoning: bool) -> Self {
        self.reasoning = Some(reasoning);
        self
    }

    /// Sets reasoning effort
    pub fn reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    /// Sets reasoning budget tokens
    pub fn reasoning_budget_tokens(mut self, tokens: u32) -> Self {
        self.reasoning_budget_tokens = Some(tokens);
        self
    }

    /// Sets a custom parameter (e.g., num_ctx for Ollama)
    pub fn parameter<K: Into<String>>(mut self, key: K, value: impl Into<Value>) -> Self {
        if self.custom.is_none() {
            self.custom = Some(HashMap::new());
        }
        if let Some(custom) = &mut self.custom {
            custom.insert(key.into(), value.into());
        }
        self
    }

    /// Converts params to JSON value for storage
    pub fn to_json(&self) -> Result<Value, serde_json::Error> {
        serde_json::to_value(self)
    }
}
