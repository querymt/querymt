//! Model and provider parametrization heuristics.
//!
//! Computes sensible default sampling parameters and provider-specific options
//! based on model and provider identity. User-explicit values always take precedence;
//! these heuristics only fill in gaps.
//!
//! The heuristics are derived from empirical best practices across model families
//! (Qwen, Gemini, GLM, MiniMax, Kimi, GPT-5, Claude, etc.) and provider-specific
//! API requirements (OpenAI store flag, Google thinkingConfig, etc.).

use querymt::providers::ModelInfo;
use serde_json::{Value, json};
use std::collections::HashMap;

/// Output-token cap used when the configuration does not set `max_tokens`
/// and the model does not advertise a smaller output limit.
const DEFAULT_OUTPUT_CAP: u32 = 32_000;

/// Output-token caps for a session: where requests start and how far an
/// escalation may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputCaps {
    /// Cap sent with ordinary requests.
    pub initial: u32,
    /// Highest cap escalation may request. Never below `initial`.
    pub ceiling: u32,
}

/// Heuristic defaults for a given provider + model combination.
///
/// All fields are optional — `None` means "no opinion, let the provider decide".
/// Use [`ModelDefaults::for_model`] to compute defaults, then [`ModelDefaults::apply_to`]
/// to merge them into a config JSON without overwriting user-explicit values.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelDefaults {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub max_tokens: Option<u32>,
    /// Model output-token limit from model metadata, when known.
    pub output_limit: Option<u32>,
    /// Provider-specific structured options (e.g. `extra_body`, `usage`, `reasoning_effort`).
    pub provider_options: HashMap<String, Value>,
    /// Provider ID for provider-specific transformations
    pub provider: String,
}

impl ModelDefaults {
    /// Compute heuristic defaults for a provider + model pair.
    ///
    /// `model_info` supplies the model's output limit, which bounds the default
    /// `max_tokens` and becomes the escalation ceiling (see [`Self::output_caps`]).
    pub fn for_model(provider: &str, model: &str, model_info: Option<&ModelInfo>) -> Self {
        let output_limit = model_info
            .and_then(|info| info.limits.output)
            .and_then(|limit| u32::try_from(limit).ok())
            .filter(|&limit| limit > 0);
        Self {
            temperature: default_temperature(model),
            top_p: default_top_p(model),
            top_k: default_top_k(model),
            max_tokens: default_max_tokens(provider, output_limit),
            output_limit,
            provider_options: default_provider_options(provider, model),
            provider: provider.to_string(),
        }
    }

    /// Whether model metadata can change the output caps for `provider`.
    ///
    /// Metadata lookups parse the models.dev cache, so callers should skip them
    /// when this is `false`: an explicit `max_tokens` is its own ceiling, and a
    /// provider without a default cap has nothing to escalate from.
    pub fn wants_model_info(provider: &str, config: &Value) -> bool {
        default_max_tokens(provider, None).is_some() && config.get("max_tokens").is_none()
    }

    /// Output caps that result from applying these defaults to `config`.
    ///
    /// Call this with the config *before* [`Self::apply_to`]: an explicit
    /// `max_tokens` is used as-is and is its own ceiling, so escalation never
    /// exceeds an explicit choice. Otherwise the cap starts at the default and
    /// may escalate up to the model's output limit. Returns `None` when no cap
    /// is known (the provider decides), in which case there is nothing to escalate.
    pub fn output_caps(&self, config: &Value) -> Option<OutputCaps> {
        let explicit = config
            .get("max_tokens")
            .and_then(Value::as_u64)
            .and_then(|tokens| u32::try_from(tokens).ok());
        if let Some(cap) = explicit {
            return Some(OutputCaps {
                initial: cap,
                ceiling: cap,
            });
        }
        let initial = self.max_tokens?;
        Some(OutputCaps {
            initial,
            ceiling: self
                .output_limit
                .map_or(initial, |limit| limit.max(initial)),
        })
    }

    /// Apply these defaults to a JSON config object.
    ///
    /// Only fills keys that are **not already present** — user-explicit values always win.
    /// `session_id` is used by providers that support prompt caching (e.g. OpenAI).
    pub fn apply_to(&self, config: &mut Value, session_id: &str) {
        let obj = match config.as_object_mut() {
            Some(obj) => obj,
            None => return,
        };

        if !obj.contains_key("temperature")
            && let Some(t) = self.temperature
        {
            obj.insert("temperature".into(), json!(t));
        }

        if !obj.contains_key("top_p")
            && let Some(p) = self.top_p
        {
            obj.insert("top_p".into(), json!(p));
        }

        if !obj.contains_key("top_k")
            && let Some(k) = self.top_k
        {
            obj.insert("top_k".into(), json!(k));
        }

        if !obj.contains_key("max_tokens")
            && let Some(m) = self.max_tokens
        {
            obj.insert("max_tokens".into(), json!(m));
        }

        for (key, value) in &self.provider_options {
            if !obj.contains_key(key) {
                let mut v = value.clone();
                substitute_session_id(&mut v, session_id);
                // Responses owns storage policy; do not inject its reserved field.
                if key == "extra_body"
                    && self.provider == "openai"
                    && obj.get("api_mode").and_then(Value::as_str) == Some("responses")
                    && let Some(extra) = v.as_object_mut()
                {
                    extra.remove("store");
                }
                obj.insert(key.clone(), v);
            }
        }

        // Configured instructions normally form stable prefixes. Providers own
        // capability checks and TTL translation; unsupported endpoints skip hints.
        if self.provider == "anthropic"
            || self.provider == "codex"
            || (self.provider == "openai"
                && obj.get("api_mode").and_then(Value::as_str) == Some("responses"))
        {
            self.apply_system_cache_defaults(obj);
        }
    }

    /// Default each configured instruction section to provider-default lifetime.
    /// Explicit hints (including null) and native controls always take precedence.
    fn apply_system_cache_defaults(&self, obj: &mut serde_json::Map<String, Value>) {
        let Some(system) = obj.get("system") else {
            return;
        };
        let parts = match system {
            Value::Array(parts) => parts.clone(),
            Value::String(text) => vec![Value::String(text.clone())],
            Value::Object(_) => vec![system.clone()],
            _ => return,
        };
        let parts = parts
            .into_iter()
            .map(|part| match part {
                Value::String(text) => json!({"text": text, "cache": {"ephemeral": {}}}),
                Value::Object(mut part) => {
                    // Explicit legacy provider controls remain an escape hatch.
                    if !part.contains_key("cache_control") {
                        part.entry("cache").or_insert(json!({"ephemeral": {}}));
                    }
                    Value::Object(part)
                }
                other => other,
            })
            .collect();
        obj.insert("system".into(), Value::Array(parts));
    }

    /// Returns true if all fields are None/empty (no heuristics apply).
    pub fn is_empty(&self) -> bool {
        self.temperature.is_none()
            && self.top_p.is_none()
            && self.top_k.is_none()
            && self.max_tokens.is_none()
            && self.provider_options.is_empty()
        // provider is metadata, not a default value, so don't check it
    }
}

// ---------------------------------------------------------------------------
// Session ID placeholder substitution
// ---------------------------------------------------------------------------

/// Recursively replace `"__session_id__"` string placeholders in a JSON value.
fn substitute_session_id(value: &mut Value, session_id: &str) {
    match value {
        Value::String(s) if s == "__session_id__" => {
            *value = json!(session_id);
        }
        Value::Object(map) => {
            for v in map.values_mut() {
                substitute_session_id(v, session_id);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                substitute_session_id(v, session_id);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Sampling parameter heuristics (model-level, keyed on model ID)
// ---------------------------------------------------------------------------

fn default_temperature(model: &str) -> Option<f32> {
    let id = model.to_lowercase();

    if id.contains("qwen") {
        return Some(0.55);
    }
    // Claude: leave unset. The Anthropic provider omits `temperature` (and
    // `top_p`/`top_k`) whenever the model rejects them (Opus 4.7+, Claude 5+,
    // Fable, Mythos) or when thinking is sent, so a default here would only be
    // dropped or risk a rejected request.
    if id.contains("claude") {
        return None;
    }
    if id.contains("gemini") {
        return Some(1.0);
    }
    if id.contains("glm-4.6") || id.contains("glm-4.7") {
        return Some(1.0);
    }
    if id.contains("minimax-m2") {
        return Some(1.0);
    }
    if id.contains("kimi-k2") {
        // kimi-k2-thinking & kimi-k2.5 variants
        if id.contains("thinking") || id.contains("k2.") {
            return Some(1.0);
        }
        return Some(0.6);
    }

    None
}

fn default_top_p(model: &str) -> Option<f32> {
    let id = model.to_lowercase();

    if id.contains("qwen") {
        return Some(1.0);
    }
    if id.contains("minimax-m2") || id.contains("kimi-k2.5") || id.contains("gemini") {
        return Some(0.95);
    }

    None
}

fn default_top_k(model: &str) -> Option<u32> {
    let id = model.to_lowercase();

    if id.contains("minimax-m2") {
        if id.contains("m2.1") {
            return Some(40);
        }
        return Some(20);
    }
    if id.contains("gemini") {
        return Some(64);
    }

    None
}

fn default_max_tokens(provider: &str, output_limit: Option<u32>) -> Option<u32> {
    // Anthropic requires max_tokens to be set explicitly
    (provider == "anthropic")
        .then(|| output_limit.map_or(DEFAULT_OUTPUT_CAP, |limit| limit.min(DEFAULT_OUTPUT_CAP)))
}

// ---------------------------------------------------------------------------
// Provider-specific option heuristics
// ---------------------------------------------------------------------------

fn default_provider_options(provider: &str, model: &str) -> HashMap<String, Value> {
    let mut opts = HashMap::new();
    let id = model.to_lowercase();

    // A stable session key improves prompt-cache routing for OpenAI and Codex.
    // Only Chat Completions needs a storage default here; Responses owns it.
    if provider == "openai" || provider == "codex" {
        let mut extra = json!({"prompt_cache_key": "__session_id__"});
        if provider == "openai" {
            extra["store"] = json!(false);
        }
        opts.insert("extra_body".into(), extra);
    }

    // OpenRouter: request usage stats
    if provider == "openrouter" {
        opts.insert("usage".into(), json!({"include": true}));
    }

    // Claude is intentionally absent here: session effort "auto" stays unset
    // and the Anthropic provider resolves it per model class (default-on
    // adaptive models keep API defaults, opt-in adaptive models get adaptive
    // thinking, budget models get none). Inserting an effort would turn "auto"
    // into an explicit effort in stored configs and the session UI.

    // Default reasoning effort for model families that benefit from it.
    if id.contains("gemini-3") {
        opts.insert("reasoning_effort".into(), json!("high"));
    }

    // GPT-5 family heuristics
    if id.contains("gpt-5") && !id.contains("gpt-5-chat") {
        if !id.contains("gpt-5-pro") {
            opts.insert("reasoning_effort".into(), json!("medium"));
        }
        if id.contains("gpt-5.")
            && !id.contains("codex")
            && provider != "azure"
            && provider != "codex"
        {
            let eb = opts.entry("extra_body".into()).or_insert_with(|| json!({}));
            eb.as_object_mut()
                .unwrap()
                .insert("verbosity".into(), json!("medium"));
        }
    }

    if provider.to_lowercase().contains("zai") || id.contains("glm-5") {
        opts.insert(
            "extra_body".into(),
            json!({"thinking": {"type": "enabled","clear_thinking": false}}),
        );
    }

    opts
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Output caps (initial cap + escalation ceiling)
    // -----------------------------------------------------------------------

    fn info_with_output_limit(limit: u64) -> ModelInfo {
        let mut info = ModelInfo::default();
        info.limits.output = Some(limit);
        info
    }

    #[test]
    fn output_caps_for_large_output_model_start_at_32k_with_model_ceiling() {
        let info = info_with_output_limit(128_000);
        let d = ModelDefaults::for_model("anthropic", "claude-sonnet-5-5", Some(&info));
        assert_eq!(d.max_tokens, Some(32_000));
        assert_eq!(
            d.output_caps(&json!({})),
            Some(OutputCaps {
                initial: 32_000,
                ceiling: 128_000
            })
        );
    }

    #[test]
    fn output_caps_clamp_initial_cap_to_small_output_limit() {
        let info = info_with_output_limit(8_192);
        let d = ModelDefaults::for_model("anthropic", "claude-3-haiku", Some(&info));
        assert_eq!(
            d.output_caps(&json!({})),
            Some(OutputCaps {
                initial: 8_192,
                ceiling: 8_192
            })
        );
    }

    #[test]
    fn output_caps_for_unknown_anthropic_model_use_32k_for_both() {
        let d = ModelDefaults::for_model("anthropic", "claude-unknown", None);
        assert_eq!(
            d.output_caps(&json!({})),
            Some(OutputCaps {
                initial: 32_000,
                ceiling: 32_000
            })
        );
    }

    #[test]
    fn explicit_max_tokens_is_its_own_ceiling() {
        let info = info_with_output_limit(128_000);
        let d = ModelDefaults::for_model("anthropic", "claude-sonnet-5-5", Some(&info));
        assert_eq!(
            d.output_caps(&json!({"max_tokens": 20_000})),
            Some(OutputCaps {
                initial: 20_000,
                ceiling: 20_000
            })
        );
    }

    #[test]
    fn model_info_is_only_wanted_when_it_can_change_the_caps() {
        assert!(ModelDefaults::wants_model_info("anthropic", &json!({})));
        assert!(!ModelDefaults::wants_model_info(
            "anthropic",
            &json!({"max_tokens": 20_000})
        ));
        assert!(!ModelDefaults::wants_model_info("openai", &json!({})));
    }

    #[test]
    fn output_caps_are_unknown_without_any_cap_source() {
        let d = ModelDefaults::for_model("openai", "gpt-4o", None);
        assert_eq!(d.output_caps(&json!({})), None);
    }

    #[test]
    fn non_anthropic_model_with_output_limit_keeps_provider_default_cap() {
        let info = info_with_output_limit(16_384);
        let d = ModelDefaults::for_model("openai", "gpt-4o", Some(&info));
        assert_eq!(d.max_tokens, None);
        assert_eq!(d.output_limit, Some(16_384));
    }

    // -----------------------------------------------------------------------
    // Temperature heuristics
    // -----------------------------------------------------------------------

    #[test]
    fn test_qwen_temperature() {
        let d = ModelDefaults::for_model("alibaba", "qwen-2.5-coder", None);
        assert_eq!(d.temperature, Some(0.55));
    }

    #[test]
    fn test_claude_no_temperature() {
        let d = ModelDefaults::for_model("anthropic", "claude-sonnet-4-20250514", None);
        assert_eq!(d.temperature, None);
    }

    #[test]
    fn test_gemini_temperature() {
        let d = ModelDefaults::for_model("google", "gemini-2.5-pro", None);
        assert_eq!(d.temperature, Some(1.0));
    }

    #[test]
    fn test_glm_temperature() {
        let d = ModelDefaults::for_model("zai", "glm-4.6", None);
        assert_eq!(d.temperature, Some(1.0));
        let d = ModelDefaults::for_model("zai", "glm-4.7", None);
        assert_eq!(d.temperature, Some(1.0));
    }

    #[test]
    fn test_minimax_temperature() {
        let d = ModelDefaults::for_model("minimax", "minimax-m2", None);
        assert_eq!(d.temperature, Some(1.0));
    }

    #[test]
    fn test_kimi_k2_base_temperature() {
        let d = ModelDefaults::for_model("moonshot", "kimi-k2", None);
        assert_eq!(d.temperature, Some(0.6));
    }

    #[test]
    fn test_kimi_k2_thinking_temperature() {
        let d = ModelDefaults::for_model("moonshot", "kimi-k2-thinking", None);
        assert_eq!(d.temperature, Some(1.0));
    }

    #[test]
    fn test_kimi_k2_5_temperature() {
        let d = ModelDefaults::for_model("moonshot", "kimi-k2.5", None);
        assert_eq!(d.temperature, Some(1.0));
    }

    #[test]
    fn test_unknown_model_no_temperature() {
        let d = ModelDefaults::for_model("some-provider", "some-model", None);
        assert_eq!(d.temperature, None);
    }

    // -----------------------------------------------------------------------
    // Top-p heuristics
    // -----------------------------------------------------------------------

    #[test]
    fn test_qwen_top_p() {
        let d = ModelDefaults::for_model("alibaba", "qwen-2.5-coder", None);
        assert_eq!(d.top_p, Some(1.0));
    }

    #[test]
    fn test_gemini_top_p() {
        let d = ModelDefaults::for_model("google", "gemini-2.5-pro", None);
        assert_eq!(d.top_p, Some(0.95));
    }

    #[test]
    fn test_minimax_top_p() {
        let d = ModelDefaults::for_model("minimax", "minimax-m2", None);
        assert_eq!(d.top_p, Some(0.95));
    }

    #[test]
    fn test_kimi_k2_5_top_p() {
        let d = ModelDefaults::for_model("moonshot", "kimi-k2.5", None);
        assert_eq!(d.top_p, Some(0.95));
    }

    #[test]
    fn test_claude_no_top_p() {
        let d = ModelDefaults::for_model("anthropic", "claude-sonnet-4-20250514", None);
        assert_eq!(d.top_p, None);
    }

    // -----------------------------------------------------------------------
    // Top-k heuristics
    // -----------------------------------------------------------------------

    #[test]
    fn test_minimax_m2_top_k() {
        let d = ModelDefaults::for_model("minimax", "minimax-m2", None);
        assert_eq!(d.top_k, Some(20));
    }

    #[test]
    fn test_minimax_m2_1_top_k() {
        let d = ModelDefaults::for_model("minimax", "minimax-m2.1", None);
        assert_eq!(d.top_k, Some(40));
    }

    #[test]
    fn test_gemini_top_k() {
        let d = ModelDefaults::for_model("google", "gemini-2.5-pro", None);
        assert_eq!(d.top_k, Some(64));
    }

    #[test]
    fn test_claude_no_top_k() {
        let d = ModelDefaults::for_model("anthropic", "claude-sonnet-4-20250514", None);
        assert_eq!(d.top_k, None);
    }

    // -----------------------------------------------------------------------
    // Max tokens heuristics
    // -----------------------------------------------------------------------

    #[test]
    fn test_anthropic_max_tokens() {
        let d = ModelDefaults::for_model("anthropic", "claude-sonnet-4-20250514", None);
        assert_eq!(d.max_tokens, Some(32_000));
    }

    #[test]
    fn test_anthropic_defaults_never_insert_reasoning_effort() {
        for model in [
            "claude-sonnet-4-20250514",
            "claude-sonnet-4-5",
            "claude-sonnet-4-6",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "claude-mythos-preview",
            "us.anthropic.claude-opus-4-7-v1:0",
        ] {
            let d = ModelDefaults::for_model("anthropic", model, None);
            assert!(
                !d.provider_options.contains_key("reasoning_effort"),
                "{model}: auto must be resolved by the provider"
            );
            assert_eq!(d.temperature, None, "{model}");
        }
    }

    #[test]
    fn test_openai_no_max_tokens() {
        let d = ModelDefaults::for_model("openai", "gpt-4o", None);
        assert_eq!(d.max_tokens, None);
    }

    // -----------------------------------------------------------------------
    // Provider options heuristics
    // -----------------------------------------------------------------------

    #[test]
    fn test_openai_extra_body_store_false() {
        let d = ModelDefaults::for_model("openai", "gpt-4o", None);
        let extra = d.provider_options.get("extra_body").unwrap();
        assert_eq!(extra["store"], json!(false));
    }

    #[test]
    fn test_openai_extra_body_prompt_cache_key() {
        let d = ModelDefaults::for_model("openai", "gpt-4o", None);
        let extra = d.provider_options.get("extra_body").unwrap();
        assert_eq!(extra["prompt_cache_key"], json!("__session_id__"));
    }

    #[test]
    fn test_google_no_raw_thinking_config_heuristic() {
        let d = ModelDefaults::for_model("google", "gemini-2.5-pro", None);
        assert!(!d.provider_options.contains_key("thinkingConfig"));
    }

    #[test]
    fn test_google_gemini3_sets_reasoning_effort() {
        let d = ModelDefaults::for_model("google", "gemini-3-pro", None);
        assert_eq!(
            d.provider_options.get("reasoning_effort"),
            Some(&json!("high"))
        );
    }

    #[test]
    fn test_openrouter_usage() {
        let d = ModelDefaults::for_model("openrouter", "some-model", None);
        assert_eq!(
            d.provider_options.get("usage"),
            Some(&json!({"include": true}))
        );
    }

    #[test]
    fn test_openrouter_gemini3_sets_reasoning_effort() {
        let d = ModelDefaults::for_model("openrouter", "gemini-3-pro", None);
        assert_eq!(
            d.provider_options.get("reasoning_effort"),
            Some(&json!("high"))
        );
    }

    #[test]
    fn test_zai_thinking() {
        let d = ModelDefaults::for_model("zai", "glm-4.6", None);
        let extra = d.provider_options.get("extra_body").unwrap();
        assert_eq!(
            extra.get("thinking"),
            Some(&json!({"type": "enabled", "clear_thinking": false}))
        );
    }

    #[test]
    fn test_gpt5_reasoning_effort() {
        let d = ModelDefaults::for_model("openai", "gpt-5", None);
        let extra = d.provider_options.get("extra_body").unwrap();
        assert_eq!(
            d.provider_options.get("reasoning_effort"),
            Some(&json!("medium"))
        );
        // OpenAI store/prompt_cache_key should still be present
        assert_eq!(extra["store"], json!(false));
        assert_eq!(extra["prompt_cache_key"], json!("__session_id__"));
    }

    #[test]
    fn test_gpt5_pro_no_reasoning_effort() {
        let d = ModelDefaults::for_model("openai", "gpt-5-pro", None);
        let extra = d.provider_options.get("extra_body").unwrap();
        assert!(!d.provider_options.contains_key("reasoning_effort"));
        // OpenAI defaults should still be present
        assert_eq!(extra["store"], json!(false));
    }

    #[test]
    fn test_gpt5_chat_no_reasoning_effort() {
        let d = ModelDefaults::for_model("openai", "gpt-5-chat", None);
        assert!(!d.provider_options.contains_key("reasoning_effort"));
    }

    #[test]
    fn test_gpt5_dot_verbosity() {
        let d = ModelDefaults::for_model("openai", "gpt-5.1", None);
        let extra = d.provider_options.get("extra_body").unwrap();
        assert_eq!(extra["verbosity"], json!("medium"));
        // Should also have reasoning effort default and OpenAI defaults
        assert_eq!(
            d.provider_options.get("reasoning_effort"),
            Some(&json!("medium"))
        );
        assert_eq!(extra["store"], json!(false));
        assert_eq!(extra["prompt_cache_key"], json!("__session_id__"));
    }

    #[test]
    fn test_gpt5_dot_azure_no_verbosity() {
        let d = ModelDefaults::for_model("azure", "gpt-5.1", None);
        // Azure doesn't get OpenAI defaults, but does get reasoning effort default
        assert_eq!(
            d.provider_options.get("reasoning_effort"),
            Some(&json!("medium"))
        );
    }

    #[test]
    fn test_gpt5_codex_no_verbosity() {
        let d = ModelDefaults::for_model("openai", "gpt-5-codex", None);
        let extra = d.provider_options.get("extra_body").unwrap();
        assert!(extra.get("verbosity").is_none());
        // OpenAI provider should get generic reasoning_effort default
        assert_eq!(
            d.provider_options.get("reasoning_effort"),
            Some(&json!("medium"))
        );
        assert_eq!(extra["store"], json!(false));
    }

    #[test]
    fn test_gpt5_dot_codex_provider_no_verbosity() {
        let d = ModelDefaults::for_model("codex", "gpt-5.2", None);
        let extra = d.provider_options.get("extra_body").unwrap();
        assert!(extra.get("verbosity").is_none());
        // Codex provider should get generic reasoning_effort default
        assert_eq!(
            d.provider_options.get("reasoning_effort"),
            Some(&json!("medium"))
        );
    }

    #[test]
    fn test_unknown_provider_no_options() {
        let d = ModelDefaults::for_model("some-provider", "some-model", None);
        assert!(d.provider_options.is_empty());
    }

    // -----------------------------------------------------------------------
    // apply_to: user overrides win
    // -----------------------------------------------------------------------

    #[test]
    fn test_apply_to_respects_user_temperature() {
        let d = ModelDefaults::for_model("alibaba", "qwen-2.5-coder", None);
        assert_eq!(d.temperature, Some(0.55));

        let mut config = json!({"model": "qwen-2.5-coder", "temperature": 0.8});
        d.apply_to(&mut config, "session-1");

        // User's 0.8 wins over heuristic 0.55
        assert_eq!(config["temperature"], json!(0.8));
    }

    #[test]
    fn test_apply_to_fills_missing_temperature() {
        let d = ModelDefaults::for_model("alibaba", "qwen-2.5-coder", None);
        let mut config = json!({"model": "qwen-2.5-coder"});
        d.apply_to(&mut config, "session-1");

        // Compare as f32 to avoid f32→f64 precision mismatch in JSON
        assert_eq!(config["temperature"].as_f64().unwrap() as f32, 0.55_f32);
    }

    #[test]
    fn test_apply_to_fills_all_sampling_params() {
        let d = ModelDefaults::for_model("google", "gemini-2.5-pro", None);
        let mut config = json!({"model": "gemini-2.5-pro"});
        d.apply_to(&mut config, "session-1");

        assert_eq!(config["temperature"].as_f64().unwrap() as f32, 1.0_f32);
        assert_eq!(config["top_p"].as_f64().unwrap() as f32, 0.95_f32);
        assert_eq!(config["top_k"], json!(64));
    }

    #[test]
    fn test_apply_to_substitutes_session_id_in_extra_body() {
        let d = ModelDefaults::for_model("openai", "gpt-4o", None);
        let mut config = json!({"model": "gpt-4o"});
        d.apply_to(&mut config, "sess-abc-123");

        assert_eq!(
            config["extra_body"]["prompt_cache_key"],
            json!("sess-abc-123")
        );
        assert_eq!(config["extra_body"]["store"], json!(false));
    }

    #[test]
    fn test_openai_responses_heuristics_build_cacheable_request() {
        let defaults = ModelDefaults::for_model("openai", "gpt-5", None);
        let messages = vec![querymt::chat::ChatMessage::user().text("hello").build()];
        for session_id in ["session-1", "session-1", "session-2"] {
            let mut config = json!({
                "model": "gpt-5", "api_key": "sk-test", "api_mode": "responses"
            });
            defaults.apply_to(&mut config, session_id);
            assert!(config["extra_body"].get("store").is_none());
            let provider: qmt_openai::OpenAI = serde_json::from_value(config).unwrap();
            let request = qmt_openai::api::openai_responses_request(&provider, &messages, None)
                .expect("heuristic defaults must not conflict with Responses policy");
            let raw = std::str::from_utf8(request.body()).unwrap();
            let body: Value = serde_json::from_slice(request.body()).unwrap();
            assert_eq!(body["prompt_cache_key"], session_id);
            assert!(body.get("promptCacheKey").is_none());
            assert_eq!(body["store"], false);
            assert_eq!(raw.matches("\"store\":").count(), 1);
        }
    }

    #[test]
    fn test_codex_apply_to_omits_storage_default() {
        let defaults = ModelDefaults::for_model("codex", "gpt-5.1-codex", None);
        let mut config = json!({"model": "gpt-5.1-codex"});
        defaults.apply_to(&mut config, "session-1");
        assert_eq!(config["extra_body"]["prompt_cache_key"], "session-1");
        assert!(config["extra_body"].get("store").is_none());
    }

    #[test]
    fn test_responses_preserves_user_extra_body_conflicts() {
        let defaults = ModelDefaults::for_model("openai", "gpt-5", None);
        let mut config = json!({
            "model": "gpt-5", "api_key": "sk-test", "api_mode": "responses",
            "extra_body": {"promptCacheKey": "custom-key", "store": false}
        });
        defaults.apply_to(&mut config, "session-1");
        assert_eq!(config["extra_body"]["promptCacheKey"], "custom-key");
        assert_eq!(config["extra_body"]["store"], false);
        let provider: qmt_openai::OpenAI = serde_json::from_value(config).unwrap();
        let messages = vec![querymt::chat::ChatMessage::user().text("hello").build()];
        let err = qmt_openai::api::openai_responses_request(&provider, &messages, None)
            .expect_err("user-supplied reserved fields must still be rejected");
        assert!(err.to_string().contains("protocol-owned field 'store'"));
    }

    #[test]
    fn test_apply_to_user_extra_body_wins() {
        let d = ModelDefaults::for_model("openai", "gpt-4o", None);
        let mut config = json!({
            "model": "gpt-4o",
            "extra_body": { "prompt_cache_key": "custom-key", "store": true }
        });
        d.apply_to(&mut config, "sess-abc-123");

        // User's extra_body wins entirely over heuristic extra_body
        assert_eq!(
            config["extra_body"]["prompt_cache_key"],
            json!("custom-key")
        );
        assert_eq!(config["extra_body"]["store"], json!(true));
    }

    #[test]
    fn test_apply_to_non_object_is_noop() {
        let d = ModelDefaults::for_model("google", "gemini-2.5-pro", None);
        let mut config = json!("not-an-object");
        d.apply_to(&mut config, "session-1");

        assert_eq!(config, json!("not-an-object"));
    }

    // -----------------------------------------------------------------------
    // is_empty
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_empty_for_unknown() {
        let d = ModelDefaults::for_model("unknown", "unknown", None);
        assert!(d.is_empty());
    }

    #[test]
    fn test_is_not_empty_for_known() {
        let d = ModelDefaults::for_model("google", "gemini-2.5-pro", None);
        assert!(!d.is_empty());
    }

    // -----------------------------------------------------------------------
    // Anthropic system prompt caching tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_anthropic_system_caching_string() {
        let defaults = ModelDefaults::for_model("anthropic", "claude-3-7-sonnet-20250219", None);
        let mut config = json!({"system": "You are a helpful assistant."});
        defaults.apply_to(&mut config, "session-1");
        assert_eq!(
            config["system"],
            json!([{
                "text": "You are a helpful assistant.", "cache": {"ephemeral": {}}
            }])
        );
    }

    #[test]
    fn test_anthropic_system_caching_string_array() {
        let defaults = ModelDefaults::for_model("anthropic", "claude-3-7-sonnet-20250219", None);
        let mut config = json!({"system": ["Part 1", "Part 2"]});
        defaults.apply_to(&mut config, "session-1");
        assert_eq!(
            config["system"],
            json!([
                {"text": "Part 1", "cache": {"ephemeral": {}}},
                {"text": "Part 2", "cache": {"ephemeral": {}}}
            ])
        );
    }

    #[test]
    fn anthropic_defaults_preserve_portable_instruction_hints() {
        let defaults = ModelDefaults::for_model("anthropic", "claude-3-7-sonnet-20250219", None);
        let instruction = json!({"text": "stable", "cache": {"ephemeral": {"ttl_seconds": 1800}}});
        let mut config = json!({"system": [instruction.clone()]});
        defaults.apply_to(&mut config, "session");
        assert_eq!(config["system"][0], instruction);
        assert!(config["system"][0].get("cache_control").is_none());
    }

    #[test]
    fn test_anthropic_system_caching_existing_blocks() {
        let defaults = ModelDefaults::for_model("anthropic", "claude-3-7-sonnet-20250219", None);
        let mut config = json!({"system": [{"type": "text", "text": "Already a block"}]});
        defaults.apply_to(&mut config, "session-1");
        assert_eq!(
            config["system"][0],
            json!({
                "type": "text", "text": "Already a block", "cache": {"ephemeral": {}}
            })
        );
    }

    #[test]
    fn test_anthropic_system_caching_preserves_existing_cache_control() {
        let d = ModelDefaults::for_model("anthropic", "claude-3-7-sonnet-20250219", None);
        let mut config = json!({
            "model": "claude-3-7-sonnet-20250219",
            "system": [
                {
                    "type": "text",
                    "text": "Block with existing cache",
                    "cache_control": { "type": "ephemeral", "ttl": "1h" }
                }
            ]
        });
        d.apply_to(&mut config, "session-1");

        // Should preserve existing cache_control with ttl
        let blocks = config["system"].as_array().unwrap();
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(blocks[0]["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn stable_system_sections_get_portable_defaults_for_responses_and_codex() {
        for provider in ["openai", "codex"] {
            let defaults = ModelDefaults::for_model(provider, "gpt-6.1-sol", None);
            let explicit =
                json!({"text": "explicit", "cache": {"ephemeral": {"ttl_seconds": 1800}}});
            let mut config = json!({
                "api_mode": "responses",
                "system": ["file contents", "later instructions", explicit.clone()]
            });
            defaults.apply_to(&mut config, "session");
            assert_eq!(
                config["system"][0],
                json!({"text": "file contents", "cache": {"ephemeral": {}}})
            );
            assert_eq!(
                config["system"][1],
                json!({"text": "later instructions", "cache": {"ephemeral": {}}})
            );
            assert_eq!(config["system"][2], explicit);
            let original = config.clone();
            defaults.apply_to(&mut config, "session");
            assert_eq!(config, original);
        }
    }

    #[test]
    fn default_system_hints_reach_responses_with_provider_default_ttl() {
        let defaults = ModelDefaults::for_model("openai", "gpt-5.6", None);
        let mut config = json!({"api_key": "sk-test", "model": "gpt-5.6", "api_mode": "responses", "system": ["stable", "later"]});
        defaults.apply_to(&mut config, "session");
        let provider: qmt_openai::OpenAI = serde_json::from_value(config).unwrap();
        let messages = vec![querymt::chat::ChatMessage::user().text("hello").build()];
        let request =
            qmt_openai::api::openai_responses_request(&provider, &messages, None).unwrap();
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        for index in 0..2 {
            assert_eq!(
                body["input"][0]["content"][index]["prompt_cache_breakpoint"]["mode"],
                "explicit"
            );
        }
        // Omit TTL rather than hardcoding a duration unsupported by another backend.
        assert!(body.get("prompt_cache_options").is_none());
    }

    #[test]
    fn test_non_anthropic_no_system_transformation() {
        let d = ModelDefaults::for_model("openai", "gpt-4o", None);
        let mut config = json!({
            "model": "gpt-4o",
            "system": "You are a helpful assistant."
        });
        d.apply_to(&mut config, "session-1");

        // Non-Anthropic providers should not transform system prompt
        assert!(config["system"].is_string());
        assert_eq!(config["system"], "You are a helpful assistant.");
    }

    #[test]
    fn test_codex_provider_prompt_cache_key() {
        let d = ModelDefaults::for_model("codex", "gpt-5.1-codex", None);
        let extra = d.provider_options.get("extra_body").unwrap();
        assert_eq!(extra["prompt_cache_key"], json!("__session_id__"));
        assert!(extra.get("store").is_none());
    }

    #[test]
    fn test_gpt5_codex_reasoning_object() {
        let d = ModelDefaults::for_model("codex", "gpt-5.1-codex", None);
        let extra = d.provider_options.get("extra_body").unwrap();
        assert_eq!(
            d.provider_options.get("reasoning_effort"),
            Some(&json!("medium"))
        );
        // Codex owns storage policy instead of injecting a duplicate field.
        assert!(extra.get("store").is_none());
        assert_eq!(extra["prompt_cache_key"], json!("__session_id__"));
    }

    #[test]
    fn test_gpt5_codex_max_reasoning_object() {
        let d = ModelDefaults::for_model("codex", "gpt-5.1-codex-max", None);
        let _extra = d.provider_options.get("extra_body").unwrap();
        assert_eq!(
            d.provider_options.get("reasoning_effort"),
            Some(&json!("medium"))
        );
    }
}
