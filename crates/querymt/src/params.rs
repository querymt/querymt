//! LLM configuration parameters.
//!
//! This module provides a serializable configuration struct that contains
//! only LLM parameters without operational concerns like validators or tool registries.

use crate::chat::ReasoningEffort;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Resolved application instruction content with a portable cache boundary.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum SystemPrompt {
    Text(String),
    Hinted {
        text: String,
        #[serde(default)]
        cache: Option<crate::chat::CacheHint>,
    },
}

impl PartialEq for SystemPrompt {
    fn eq(&self, other: &Self) -> bool {
        self.text() == other.text() && self.cache() == other.cache()
    }
}
impl Eq for SystemPrompt {}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
enum SystemPromptInput {
    Many(Vec<SystemPrompt>),
    One(SystemPrompt),
}

/// Schema matches the single-string, structured-part, and array input forms.
pub fn schema_for_system_prompts(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    generator.subschema_for::<Option<SystemPromptInput>>()
}

impl SystemPrompt {
    pub fn text(&self) -> &str {
        match self {
            Self::Text(text) | Self::Hinted { text, .. } => text,
        }
    }
    fn into_text(self) -> String {
        match self {
            Self::Text(text) | Self::Hinted { text, .. } => text,
        }
    }
    pub fn cache(&self) -> Option<&crate::chat::CacheHint> {
        match self {
            Self::Text(_) => None,
            Self::Hinted { cache, .. } => cache.as_ref(),
        }
    }
    pub fn with_cache(text: impl Into<String>, cache: crate::chat::CacheHint) -> Self {
        Self::Hinted {
            text: text.into(),
            cache: Some(cache),
        }
    }
}
impl Serialize for SystemPrompt {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.cache().is_none() {
            return serializer.serialize_str(self.text());
        }
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("SystemPrompt", 2)?;
        state.serialize_field("text", self.text())?;
        state.serialize_field("cache", &self.cache())?;
        state.end()
    }
}
impl From<String> for SystemPrompt {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}
impl From<&str> for SystemPrompt {
    fn from(text: &str) -> Self {
        Self::Text(text.into())
    }
}
impl From<&String> for SystemPrompt {
    fn from(text: &String) -> Self {
        Self::Text(text.clone())
    }
}
impl From<&SystemPrompt> for SystemPrompt {
    fn from(value: &SystemPrompt) -> Self {
        value.clone()
    }
}
impl std::ops::Deref for SystemPrompt {
    type Target = str;
    fn deref(&self) -> &str {
        self.text()
    }
}
impl AsRef<str> for SystemPrompt {
    fn as_ref(&self) -> &str {
        self.text()
    }
}
impl std::fmt::Display for SystemPrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.text())
    }
}
impl PartialEq<str> for SystemPrompt {
    fn eq(&self, other: &str) -> bool {
        self.text() == other
    }
}
impl PartialEq<&str> for SystemPrompt {
    fn eq(&self, other: &&str) -> bool {
        self.text() == *other
    }
}
impl PartialEq<String> for SystemPrompt {
    fn eq(&self, other: &String) -> bool {
        self.text() == other
    }
}

pub fn deserialize_system_prompts<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<SystemPrompt>, D::Error> {
    Ok(
        match Option::<SystemPromptInput>::deserialize(deserializer)? {
            None => Vec::new(),
            Some(SystemPromptInput::Many(parts)) => parts,
            Some(SystemPromptInput::One(part)) => vec![part],
        },
    )
}

/// Deserializes a single string or structured prompt, an array of either, or null.
/// String-only providers discard validated cache hints and join parts with blank lines.
/// An empty collection becomes `None`, while an empty string remains `Some("")`.
pub fn deserialize_system_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let parts = deserialize_system_vec(deserializer)?;
    Ok(if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    })
}

/// Deserializes a single string or structured prompt, an array of either, or null
/// into text parts. Providers that do not support cache hints discard them after validation.
pub fn deserialize_system_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(deserialize_system_prompts(deserializer)?
        .into_iter()
        .map(SystemPrompt::into_text)
        .collect())
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[derive(Debug, Deserialize, Default)]
    struct TextVectorConfig {
        #[serde(default, deserialize_with = "deserialize_system_vec")]
        system: Vec<String>,
    }

    #[derive(Debug, Deserialize, Default)]
    struct TextStringConfig {
        #[serde(default, deserialize_with = "deserialize_system_string")]
        system: Option<String>,
    }

    fn valid_system_inputs() -> Vec<(Value, Vec<&'static str>)> {
        vec![
            (serde_json::json!(null), vec![]),
            (serde_json::json!([]), vec![]),
            (serde_json::json!("single"), vec!["single"]),
            (serde_json::json!(""), vec![""]),
            (serde_json::json!([""]), vec![""]),
            (
                serde_json::json!(["first", "second"]),
                vec!["first", "second"],
            ),
            (serde_json::json!({"text": "plain"}), vec!["plain"]),
            (
                serde_json::json!({"text": "plain", "cache": null}),
                vec!["plain"],
            ),
            (
                serde_json::json!({"text": "cached", "cache": {"ephemeral": {}}}),
                vec!["cached"],
            ),
            (
                serde_json::json!(["legacy", {"text": "cached", "cache": {"ephemeral": {"ttl_seconds": 1800}}}]),
                vec!["legacy", "cached"],
            ),
        ]
    }

    fn invalid_system_inputs() -> Vec<Value> {
        vec![
            serde_json::json!(42),
            serde_json::json!(true),
            serde_json::json!({}),
            serde_json::json!({"text": null}),
            serde_json::json!({"text": 42}),
            serde_json::json!({"text": "bad", "unknown": true}),
            serde_json::json!({"text": "bad", "cache": {"unknown": {}}}),
            serde_json::json!({"text": "bad", "cache": {"ephemeral": {"ttl_seconds": "invalid"}}}),
            serde_json::json!([null]),
            serde_json::json!(["valid", 42]),
            serde_json::json!([["nested"]]),
            serde_json::json!([{"text": "bad", "unknown": true}]),
            serde_json::json!([{"text": "bad", "cache": {"unknown": {}}}]),
        ]
    }

    #[test]
    fn system_deserializers_accept_all_supported_shapes() {
        for (value, expected) in valid_system_inputs() {
            let config = serde_json::json!({"system": value});
            let params: LLMParams = serde_json::from_value(config.clone()).unwrap();
            let texts: Vec<_> = params.system.iter().map(SystemPrompt::text).collect();
            assert_eq!(texts, expected, "input: {value}");
            let vector: TextVectorConfig = serde_json::from_value(config.clone()).unwrap();
            assert_eq!(vector.system, expected, "input: {value}");
            let string: TextStringConfig = serde_json::from_value(config).unwrap();
            let joined = (!expected.is_empty()).then(|| expected.join("\n\n"));
            assert_eq!(string.system, joined, "input: {value}");

            // The public helper also accepts values directly, outside serde field hooks.
            assert_eq!(deserialize_system_prompts(value).unwrap(), params.system);
            let serialized = params.to_json().unwrap();
            if expected.is_empty() {
                assert!(serialized.get("system").is_none());
            } else {
                assert!(serialized["system"].is_array());
            }
            assert_eq!(
                serde_json::from_value::<LLMParams>(serialized).unwrap(),
                params
            );
        }
    }

    #[test]
    fn missing_system_fields_default_to_empty() {
        let config = serde_json::json!({});
        assert!(
            serde_json::from_value::<LLMParams>(config.clone())
                .unwrap()
                .system
                .is_empty()
        );
        assert!(
            serde_json::from_value::<TextVectorConfig>(config.clone())
                .unwrap()
                .system
                .is_empty()
        );
        assert_eq!(
            serde_json::from_value::<TextStringConfig>(config)
                .unwrap()
                .system,
            None
        );
    }

    #[test]
    fn system_deserializers_reject_invalid_parts_even_for_text_only_providers() {
        for value in invalid_system_inputs() {
            let config = serde_json::json!({"system": value});
            assert!(
                serde_json::from_value::<LLMParams>(config.clone()).is_err(),
                "input: {value}"
            );
            assert!(
                serde_json::from_value::<TextVectorConfig>(config.clone()).is_err(),
                "input: {value}"
            );
            assert!(
                serde_json::from_value::<TextStringConfig>(config).is_err(),
                "input: {value}"
            );
            assert!(
                deserialize_system_prompts(value.clone()).is_err(),
                "input: {value}"
            );
        }
    }

    #[cfg(feature = "jsonschema")]
    #[test]
    fn system_schema_matches_deserialization() {
        #[derive(Deserialize, schemars::JsonSchema)]
        struct SchemaConfig {
            #[serde(default, deserialize_with = "deserialize_system_prompts")]
            #[schemars(schema_with = "schema_for_system_prompts")]
            system: Vec<SystemPrompt>,
        }

        let schema = serde_json::to_value(schemars::schema_for!(SchemaConfig)).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let inputs = std::iter::once(serde_json::json!({}))
            .chain(
                valid_system_inputs()
                    .into_iter()
                    .map(|(value, _)| serde_json::json!({"system": value})),
            )
            .chain(
                invalid_system_inputs()
                    .into_iter()
                    .map(|value| serde_json::json!({"system": value})),
            );
        for config in inputs {
            let parsed = serde_json::from_value::<SchemaConfig>(config.clone());
            assert_eq!(
                validator.is_valid(&config),
                parsed.is_ok(),
                "input: {config}"
            );
            if let Ok(parsed) = parsed {
                assert_eq!(
                    parsed.system,
                    serde_json::from_value::<LLMParams>(config).unwrap().system
                );
            }
        }
    }
    #[test]
    fn instruction_hints_round_trip_and_legacy_strings_stay_strings() {
        let value = serde_json::json!({"system": ["legacy", {"text": "stable", "cache": {"ephemeral": {"ttl_seconds": 1800}}}]});
        let params: LLMParams = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(params.system[0].text(), "legacy");
        assert_eq!(
            params.system[1].cache(),
            Some(&crate::chat::CacheHint::Ephemeral {
                ttl_seconds: Some(1800)
            })
        );
        let serialized = params.to_json().unwrap();
        assert_eq!(serialized["system"], value["system"]);
        assert_eq!(
            serde_json::from_value::<LLMParams>(serialized).unwrap(),
            params
        );
        let params: LLMParams =
            serde_json::from_value(serde_json::json!({"system": "single"})).unwrap();
        assert_eq!(params.system.len(), 1);
        let builder =
            crate::builder::LLMBuilder::new()
                .system("legacy")
                .system(SystemPrompt::with_cache(
                    "stable",
                    crate::chat::CacheHint::Ephemeral { ttl_seconds: None },
                ));
        let built = serde_json::to_value(builder).unwrap();
        assert_eq!(built["system"][0], "legacy");
        assert_eq!(
            built["system"][1]["cache"],
            serde_json::json!({"ephemeral": {}})
        );
    }
    #[test]
    fn text_only_provider_deserializers_accept_portable_instructions() {
        let value = serde_json::json!(["legacy", {"text": "stable", "cache": {"ephemeral": {}}}]);
        let parts = deserialize_system_vec(value).unwrap();
        assert_eq!(parts, ["legacy", "stable"]);
        assert!(
            serde_json::from_value::<SystemPrompt>(
                serde_json::json!({"text": "bad", "cache": {"unknown": {}}})
            )
            .is_err()
        );
    }
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
    #[serde(
        default,
        deserialize_with = "deserialize_system_prompts",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub system: Vec<SystemPrompt>,

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

    /// Reasoning effort level (low, medium, high, max).
    /// When set, providers that support reasoning will use their own API format
    /// to configure the appropriate thinking level or budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,

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
    pub fn system(mut self, system: impl Into<SystemPrompt>) -> Self {
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

    /// Sets reasoning effort level.
    /// Providers that support reasoning will map this to their own API format.
    pub fn reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(effort);
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
