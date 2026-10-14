//! Model and provider parametrization heuristics.
//!
//! Computes sensible default sampling parameters and provider-specific options
//! based on model and provider identity. User-explicit values always take precedence;
//! these heuristics only fill in gaps.
//!
//! The heuristics are derived from empirical best practices across model families
//! (Qwen, Gemini, GLM, MiniMax, Kimi, GPT-5, Claude, etc.) and provider-specific
//! API requirements (OpenAI store flag, Google thinkingConfig, etc.).

use serde_json::{Value, json};
use std::collections::HashMap;

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
    /// Provider-specific structured options (e.g. `extra_body`, `usage`, `reasoning_effort`).
    pub provider_options: HashMap<String, Value>,
    /// Provider ID for provider-specific transformations
    pub provider: String,
}

impl ModelDefaults {
    /// Compute heuristic defaults for a provider + model pair.
    pub fn for_model(provider: &str, model: &str) -> Self {
        Self {
            temperature: default_temperature(model),
            top_p: default_top_p(model),
            top_k: default_top_k(model),
            max_tokens: default_max_tokens(provider),
            provider_options: default_provider_options(provider, model),
            provider: provider.to_string(),
        }
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
    // Claude: let Anthropic's API use its own default
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

fn default_max_tokens(provider: &str) -> Option<u32> {
    // Anthropic requires max_tokens to be set explicitly
    if provider == "anthropic" {
        return Some(32_000);
    }

    None
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
    // Temperature heuristics
    // -----------------------------------------------------------------------

    #[test]
    fn test_qwen_temperature() {
        let d = ModelDefaults::for_model("alibaba", "qwen-2.5-coder");
        assert_eq!(d.temperature, Some(0.55));
    }

    #[test]
    fn test_claude_no_temperature() {
        let d = ModelDefaults::for_model("anthropic", "claude-sonnet-4-20250514");
        assert_eq!(d.temperature, None);
    }

    #[test]
    fn test_gemini_temperature() {
        let d = ModelDefaults::for_model("google", "gemini-2.5-pro");
        assert_eq!(d.temperature, Some(1.0));
    }

    #[test]
    fn test_glm_temperature() {
        let d = ModelDefaults::for_model("zai", "glm-4.6");
        assert_eq!(d.temperature, Some(1.0));
        let d = ModelDefaults::for_model("zai", "glm-4.7");
        assert_eq!(d.temperature, Some(1.0));
    }

    #[test]
    fn test_minimax_temperature() {
        let d = ModelDefaults::for_model("minimax", "minimax-m2");
        assert_eq!(d.temperature, Some(1.0));
    }

    #[test]
    fn test_kimi_k2_base_temperature() {
        let d = ModelDefaults::for_model("moonshot", "kimi-k2");
        assert_eq!(d.temperature, Some(0.6));
    }

    #[test]
    fn test_kimi_k2_thinking_temperature() {
        let d = ModelDefaults::for_model("moonshot", "kimi-k2-thinking");
        assert_eq!(d.temperature, Some(1.0));
    }

    #[test]
    fn test_kimi_k2_5_temperature() {
        let d = ModelDefaults::for_model("moonshot", "kimi-k2.5");
        assert_eq!(d.temperature, Some(1.0));
    }

    #[test]
    fn test_unknown_model_no_temperature() {
        let d = ModelDefaults::for_model("some-provider", "some-model");
        assert_eq!(d.temperature, None);
    }

    // -----------------------------------------------------------------------
    // Top-p heuristics
    // -----------------------------------------------------------------------

    #[test]
    fn test_qwen_top_p() {
        let d = ModelDefaults::for_model("alibaba", "qwen-2.5-coder");
        assert_eq!(d.top_p, Some(1.0));
    }

    #[test]
    fn test_gemini_top_p() {
        let d = ModelDefaults::for_model("google", "gemini-2.5-pro");
        assert_eq!(d.top_p, Some(0.95));
    }

    #[test]
    fn test_minimax_top_p() {
        let d = ModelDefaults::for_model("minimax", "minimax-m2");
        assert_eq!(d.top_p, Some(0.95));
    }

    #[test]
    fn test_kimi_k2_5_top_p() {
        let d = ModelDefaults::for_model("moonshot", "kimi-k2.5");
        assert_eq!(d.top_p, Some(0.95));
    }

    #[test]
    fn test_claude_no_top_p() {
        let d = ModelDefaults::for_model("anthropic", "claude-sonnet-4-20250514");
        assert_eq!(d.top_p, None);
    }

    // -----------------------------------------------------------------------
    // Top-k heuristics
    // -----------------------------------------------------------------------

    #[test]
    fn test_minimax_m2_top_k() {
        let d = ModelDefaults::for_model("minimax", "minimax-m2");
        assert_eq!(d.top_k, Some(20));
    }

    #[test]
    fn test_minimax_m2_1_top_k() {
        let d = ModelDefaults::for_model("minimax", "minimax-m2.1");
        assert_eq!(d.top_k, Some(40));
    }

    #[test]
    fn test_gemini_top_k() {
        let d = ModelDefaults::for_model("google", "gemini-2.5-pro");
        assert_eq!(d.top_k, Some(64));
    }

    #[test]
    fn test_claude_no_top_k() {
        let d = ModelDefaults::for_model("anthropic", "claude-sonnet-4-20250514");
        assert_eq!(d.top_k, None);
    }

    // -----------------------------------------------------------------------
    // Max tokens heuristics
    // -----------------------------------------------------------------------

    #[test]
    fn test_anthropic_max_tokens() {
        let d = ModelDefaults::for_model("anthropic", "claude-sonnet-4-20250514");
        assert_eq!(d.max_tokens, Some(32_000));
    }

    #[test]
    fn test_openai_no_max_tokens() {
        let d = ModelDefaults::for_model("openai", "gpt-4o");
        assert_eq!(d.max_tokens, None);
    }

    // -----------------------------------------------------------------------
    // Provider options heuristics
    // -----------------------------------------------------------------------

    #[test]
    fn test_openai_extra_body_store_false() {
        let d = ModelDefaults::for_model("openai", "gpt-4o");
        let extra = d.provider_options.get("extra_body").unwrap();
        assert_eq!(extra["store"], json!(false));
    }

    #[test]
    fn test_openai_extra_body_prompt_cache_key() {
        let d = ModelDefaults::for_model("openai", "gpt-4o");
        let extra = d.provider_options.get("extra_body").unwrap();
        assert_eq!(extra["prompt_cache_key"], json!("__session_id__"));
    }

    #[test]
    fn test_google_no_raw_thinking_config_heuristic() {
        let d = ModelDefaults::for_model("google", "gemini-2.5-pro");
        assert!(!d.provider_options.contains_key("thinkingConfig"));
    }

    #[test]
    fn test_google_gemini3_sets_reasoning_effort() {
        let d = ModelDefaults::for_model("google", "gemini-3-pro");
        assert_eq!(
            d.provider_options.get("reasoning_effort"),
            Some(&json!("high"))
        );
    }

    #[test]
    fn test_openrouter_usage() {
        let d = ModelDefaults::for_model("openrouter", "some-model");
        assert_eq!(
            d.provider_options.get("usage"),
            Some(&json!({"include": true}))
        );
    }

    #[test]
    fn test_openrouter_gemini3_sets_reasoning_effort() {
        let d = ModelDefaults::for_model("openrouter", "gemini-3-pro");
        assert_eq!(
            d.provider_options.get("reasoning_effort"),
            Some(&json!("high"))
        );
    }

    #[test]
    fn test_zai_thinking() {
        let d = ModelDefaults::for_model("zai", "glm-4.6");
        let extra = d.provider_options.get("extra_body").unwrap();
        assert_eq!(
            extra.get("thinking"),
            Some(&json!({"type": "enabled", "clear_thinking": false}))
        );
    }

    #[test]
    fn test_gpt5_reasoning_effort() {
        let d = ModelDefaults::for_model("openai", "gpt-5");
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
        let d = ModelDefaults::for_model("openai", "gpt-5-pro");
        let extra = d.provider_options.get("extra_body").unwrap();
        assert!(!d.provider_options.contains_key("reasoning_effort"));
        // OpenAI defaults should still be present
        assert_eq!(extra["store"], json!(false));
    }

    #[test]
    fn test_gpt5_chat_no_reasoning_effort() {
        let d = ModelDefaults::for_model("openai", "gpt-5-chat");
        assert!(!d.provider_options.contains_key("reasoning_effort"));
    }

    #[test]
    fn test_gpt5_dot_verbosity() {
        let d = ModelDefaults::for_model("openai", "gpt-5.1");
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
        let d = ModelDefaults::for_model("azure", "gpt-5.1");
        // Azure doesn't get OpenAI defaults, but does get reasoning effort default
        assert_eq!(
            d.provider_options.get("reasoning_effort"),
            Some(&json!("medium"))
        );
    }

    #[test]
    fn test_gpt5_codex_no_verbosity() {
        let d = ModelDefaults::for_model("openai", "gpt-5-codex");
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
        let d = ModelDefaults::for_model("codex", "gpt-5.2");
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
        let d = ModelDefaults::for_model("some-provider", "some-model");
        assert!(d.provider_options.is_empty());
    }

    // -----------------------------------------------------------------------
    // apply_to: user overrides win
    // -----------------------------------------------------------------------

    #[test]
    fn test_apply_to_respects_user_temperature() {
        let d = ModelDefaults::for_model("alibaba", "qwen-2.5-coder");
        assert_eq!(d.temperature, Some(0.55));

        let mut config = json!({"model": "qwen-2.5-coder", "temperature": 0.8});
        d.apply_to(&mut config, "session-1");

        // User's 0.8 wins over heuristic 0.55
        assert_eq!(config["temperature"], json!(0.8));
    }

    #[test]
    fn test_apply_to_fills_missing_temperature() {
        let d = ModelDefaults::for_model("alibaba", "qwen-2.5-coder");
        let mut config = json!({"model": "qwen-2.5-coder"});
        d.apply_to(&mut config, "session-1");

        // Compare as f32 to avoid f32→f64 precision mismatch in JSON
        assert_eq!(config["temperature"].as_f64().unwrap() as f32, 0.55_f32);
    }

    #[test]
    fn test_apply_to_fills_all_sampling_params() {
        let d = ModelDefaults::for_model("google", "gemini-2.5-pro");
        let mut config = json!({"model": "gemini-2.5-pro"});
        d.apply_to(&mut config, "session-1");

        assert_eq!(config["temperature"].as_f64().unwrap() as f32, 1.0_f32);
        assert_eq!(config["top_p"].as_f64().unwrap() as f32, 0.95_f32);
        assert_eq!(config["top_k"], json!(64));
    }

    #[test]
    fn test_apply_to_substitutes_session_id_in_extra_body() {
        let d = ModelDefaults::for_model("openai", "gpt-4o");
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
        let defaults = ModelDefaults::for_model("openai", "gpt-5");
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
        let defaults = ModelDefaults::for_model("codex", "gpt-5.1-codex");
        let mut config = json!({"model": "gpt-5.1-codex"});
        defaults.apply_to(&mut config, "session-1");
        assert_eq!(config["extra_body"]["prompt_cache_key"], "session-1");
        assert!(config["extra_body"].get("store").is_none());
    }

    #[test]
    fn test_responses_preserves_user_extra_body_conflicts() {
        let defaults = ModelDefaults::for_model("openai", "gpt-5");
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
        let d = ModelDefaults::for_model("openai", "gpt-4o");
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
        let d = ModelDefaults::for_model("google", "gemini-2.5-pro");
        let mut config = json!("not-an-object");
        d.apply_to(&mut config, "session-1");

        assert_eq!(config, json!("not-an-object"));
    }

    // -----------------------------------------------------------------------
    // is_empty
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_empty_for_unknown() {
        let d = ModelDefaults::for_model("unknown", "unknown");
        assert!(d.is_empty());
    }

    #[test]
    fn test_is_not_empty_for_known() {
        let d = ModelDefaults::for_model("google", "gemini-2.5-pro");
        assert!(!d.is_empty());
    }

    // -----------------------------------------------------------------------
    // Anthropic system prompt caching tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_anthropic_system_caching_string() {
        let defaults = ModelDefaults::for_model("anthropic", "claude-3-7-sonnet-20250219");
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
        let defaults = ModelDefaults::for_model("anthropic", "claude-3-7-sonnet-20250219");
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
        let defaults = ModelDefaults::for_model("anthropic", "claude-3-7-sonnet-20250219");
        let instruction = json!({"text": "stable", "cache": {"ephemeral": {"ttl_seconds": 1800}}});
        let mut config = json!({"system": [instruction.clone()]});
        defaults.apply_to(&mut config, "session");
        assert_eq!(config["system"][0], instruction);
        assert!(config["system"][0].get("cache_control").is_none());
    }

    #[test]
    fn test_anthropic_system_caching_existing_blocks() {
        let defaults = ModelDefaults::for_model("anthropic", "claude-3-7-sonnet-20250219");
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
        let d = ModelDefaults::for_model("anthropic", "claude-3-7-sonnet-20250219");
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
            let defaults = ModelDefaults::for_model(provider, "gpt-6.1-sol");
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
        let defaults = ModelDefaults::for_model("openai", "gpt-5.6");
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
        let d = ModelDefaults::for_model("openai", "gpt-4o");
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
        let d = ModelDefaults::for_model("codex", "gpt-5.1-codex");
        let extra = d.provider_options.get("extra_body").unwrap();
        assert_eq!(extra["prompt_cache_key"], json!("__session_id__"));
        assert!(extra.get("store").is_none());
    }

    #[test]
    fn test_gpt5_codex_reasoning_object() {
        let d = ModelDefaults::for_model("codex", "gpt-5.1-codex");
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
        let d = ModelDefaults::for_model("codex", "gpt-5.1-codex-max");
        let _extra = d.provider_options.get("extra_body").unwrap();
        assert_eq!(
            d.provider_options.get("reasoning_effort"),
            Some(&json!("medium"))
        );
    }
}
