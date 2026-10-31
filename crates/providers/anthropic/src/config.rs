//! Provider configuration (`Anthropic`) and authentication-type detection.

use crate::model::{ClaudeModel, parse_claude_model};
use crate::wire::system::{AnthropicSystemPrompt, schema_for_anthropic_system};
use querymt::auth::ApiKeyResolver;
use querymt::chat::{GenerationOptions, ReasoningEffort, Tool, ToolChoice};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use url::Url;

/// Authentication type for Anthropic API
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    /// Standard API key authentication (x-api-key header)
    #[serde(rename = "api_key")]
    ApiKey,
    /// OAuth token authentication (Authorization: Bearer header)
    #[serde(rename = "oauth")]
    OAuth,
}

/// Determines the authentication type based on an explicit setting or by auto-detecting from the API key format.
///
/// If `explicit_auth_type` is `Some`, returns that value directly.
/// Otherwise, auto-detects based on the token prefix:
/// - OAuth tokens: `sk-ant-oat<digits>-...` (e.g., sk-ant-oat01-...)
/// - API keys: `sk-ant-api<digits>-...` (e.g., sk-ant-api03-...)
///
/// If the token format is unrecognized, logs a warning and defaults to API key authentication.
pub fn detect_auth_type(api_key: &str, explicit_auth_type: Option<AuthType>) -> AuthType {
    if let Some(auth_type) = explicit_auth_type {
        return auth_type;
    }

    // Check for OAuth token pattern: sk-ant-oat<digits>-
    if let Some(rest) = api_key.strip_prefix("sk-ant-oat")
        && rest.chars().next().is_some_and(|c| c.is_ascii_digit())
    {
        return AuthType::OAuth;
    }

    // Check for API key pattern: sk-ant-api<digits>-
    if let Some(rest) = api_key.strip_prefix("sk-ant-api")
        && rest.chars().next().is_some_and(|c| c.is_ascii_digit())
    {
        return AuthType::ApiKey;
    }

    // Fallback: Check for generic sk-ant- prefix (backward compatibility)
    if api_key.starts_with("sk-ant-") {
        eprintln!(
            "Warning: Anthropic token format not recognized (expected 'sk-ant-oat<N>-' or 'sk-ant-api<N>-'). \
            Defaulting to API key authentication. Consider setting 'auth_type' explicitly."
        );
        return AuthType::ApiKey;
    }

    // Token doesn't match Anthropic format at all
    eprintln!(
        "Warning: Token does not match expected Anthropic format (should start with 'sk-ant-'). \
        Defaulting to API key authentication. This may cause authentication failures."
    );
    AuthType::ApiKey
}

/// Client for interacting with Anthropic's API.
///
/// Provides methods for chat and completion requests using Anthropic's models.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Anthropic {
    pub api_key: String,
    /// Optional: Explicitly specify authentication type.
    /// If not provided, will auto-detect based on api_key format:
    /// - OAuth tokens: `sk-ant-oat<digits>-...` (e.g., sk-ant-oat01-...)
    /// - API keys: `sk-ant-api<digits>-...` (e.g., sk-ant-api03-...)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<AuthType>,
    /// Reported OAuth client version (major.minor.patch), shared by headers and billing metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code_version: Option<String>,
    pub model: String,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub timeout_seconds: Option<u64>,
    #[schemars(schema_with = "schema_for_anthropic_system")]
    pub system: Option<AnthropicSystemPrompt>,
    pub stream: Option<bool>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub tools: Option<Vec<Tool>>,
    pub tool_choice: Option<ToolChoice>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub reasoning_budget_tokens: Option<u32>,
    /// Optional override for how thinking is requested.
    ///
    /// By default the thinking mode is derived from the model ID. `true`
    /// forces opt-in adaptive thinking (`thinking.type = "adaptive"`), which is
    /// the safe choice for proxy or unrecognised model IDs that front an
    /// adaptive model. `false` forces budget thinking
    /// (`thinking.type = "enabled"` with `budget_tokens`), which the API
    /// rejects on Opus 4.7+ and every Claude 5 or later model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
    /// Names of tools sent with `strict: true` (matched against the unprefixed
    /// tool name). Unset or empty means no name-based selection; tool
    /// definitions that explicitly set `strict: true` are still honoured. Every
    /// strictness request is subject to schema, model and budget checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict_tools: Option<Vec<String>>,
    /// Optional resolver for dynamic credential refresh (e.g., OAuth tokens).
    #[serde(skip)]
    #[schemars(skip)]
    pub key_resolver: Option<Arc<dyn ApiKeyResolver>>,
}

impl Anthropic {
    pub(crate) fn default_base_url() -> Url {
        Url::parse("https://api.anthropic.com/v1/").unwrap()
    }

    /// Copy of this config with per-request overrides applied.
    ///
    /// Every downstream decision (budget fitting against `max_tokens`, adaptive
    /// effort, sampling restrictions) reads from the config, so applying the
    /// overrides up front keeps one code path for configured and per-request
    /// values. An effort override also discards a configured explicit
    /// `reasoning_budget_tokens`; otherwise that budget would silently win over
    /// the requested effort.
    pub(crate) fn with_generation_options(&self, options: &GenerationOptions) -> Self {
        let mut cfg = self.clone();
        if let Some(max_tokens) = options.max_tokens {
            cfg.max_tokens = max_tokens;
        }
        if let Some(effort) = options.reasoning_effort {
            cfg.reasoning_effort = Some(effort);
            cfg.reasoning_budget_tokens = None;
        }
        cfg
    }

    /// Parsed Claude model, or `None` for IDs that aren't recognisable.
    pub(crate) fn parsed_model(&self) -> Option<ClaudeModel> {
        parse_claude_model(&self.model)
    }
}
