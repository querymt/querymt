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
    /// Provider-specific structured options (e.g. `store`, `thinkingConfig`).
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
                // Special case: substitute session_id placeholder
                if value == "__session_id__" {
                    obj.insert(key.clone(), json!(session_id));
                } else {
                    obj.insert(key.clone(), value.clone());
                }
            }
        }

        // Apply Anthropic-specific system prompt caching transformation
        if self.provider == "anthropic" {
            self.apply_anthropic_system_caching(obj);
        }
    }

    /// Transform the system prompt for Anthropic to include cache_control on each block.
    /// Converts string/string-array system prompts into TextBlockParam objects with cache_control.
    fn apply_anthropic_system_caching(&self, obj: &mut serde_json::Map<String, Value>) {
        if let Some(system_val) = obj.get("system").cloned() {
            let blocks = match &system_val {
                Value::Array(arr) => arr
                    .iter()
                    .map(|v| {
                        if let Some(s) = v.as_str() {
                            // String element → convert to TextBlockParam with cache_control
                            json!({
                                "type": "text",
                                "text": s,
                                "cache_control": { "type": "ephemeral" }
                            })
                        } else if let Some(o) = v.as_object() {
                            // Already a TextBlockParam object → add cache_control if missing
                            let mut block = o.clone();
                            block
                                .entry("cache_control".to_string())
                                .or_insert(json!({ "type": "ephemeral" }));
                            Value::Object(block)
                        } else {
                            v.clone()
                        }
                    })
                    .collect(),
                Value::String(s) => {
                    // Single string → convert to array with one TextBlockParam
                    vec![json!({
                        "type": "text",
                        "text": s,
                        "cache_control": { "type": "ephemeral" }
                    })]
                }
                _ => return, // Unexpected type, don't transform
            };
            obj.insert("system".into(), Value::Array(blocks));
        }
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

    // OpenAI:
    //   - disable request storage by default
    //   - enable prompt caching via session ID.
    if provider == "openai" {
        opts.insert("store".into(), json!(false));
        opts.insert("promptCacheKey".into(), json!("__session_id__"));
    }

    // Google / Google Vertex: enable thinking output
    if provider == "google" {
        let mut thinking = json!({"includeThoughts": true});
        if id.contains("gemini-3") {
            thinking["thinkingLevel"] = json!("high");
        }
        opts.insert("thinkingConfig".into(), thinking);
    }

    // OpenRouter: request usage stats
    if provider == "openrouter" {
        opts.insert("usage".into(), json!({"include": true}));
        if id.contains("gemini-3") {
            opts.insert("reasoning".into(), json!({"effort": "high"}));
        }
    }

    // GPT-5 family heuristics
    if id.contains("gpt-5") && !id.contains("gpt-5-chat") {
        if !id.contains("gpt-5-pro") {
            opts.insert("reasoningEffort".into(), json!("medium"));
        }
        if id.contains("gpt-5.") && !id.contains("codex") && provider != "azure" {
            opts.insert("textVerbosity".into(), json!("low"));
        }
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
        let d = ModelDefaults::for_model("zhipuai", "glm-4.6");
        assert_eq!(d.temperature, Some(1.0));
        let d = ModelDefaults::for_model("zhipuai", "glm-4.7");
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
    fn test_openai_store_false() {
        let d = ModelDefaults::for_model("openai", "gpt-4o");
        assert_eq!(d.provider_options.get("store"), Some(&json!(false)));
    }

    #[test]
    fn test_openai_prompt_cache_key() {
        let d = ModelDefaults::for_model("openai", "gpt-4o");
        assert_eq!(
            d.provider_options.get("promptCacheKey"),
            Some(&json!("__session_id__"))
        );
    }

    #[test]
    fn test_google_thinking_config() {
        let d = ModelDefaults::for_model("google", "gemini-2.5-pro");
        assert_eq!(
            d.provider_options.get("thinkingConfig"),
            Some(&json!({"includeThoughts": true}))
        );
    }

    #[test]
    fn test_google_gemini3_thinking_level() {
        let d = ModelDefaults::for_model("google", "gemini-3-pro");
        assert_eq!(
            d.provider_options.get("thinkingConfig"),
            Some(&json!({"includeThoughts": true, "thinkingLevel": "high"}))
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
    fn test_openrouter_gemini3_reasoning() {
        let d = ModelDefaults::for_model("openrouter", "gemini-3-pro");
        assert_eq!(
            d.provider_options.get("reasoning"),
            Some(&json!({"effort": "high"}))
        );
    }

    // TODO: enable once zhipuai provider is added
    #[test]
    #[ignore]
    fn test_zhipuai_thinking() {
        let d = ModelDefaults::for_model("zhipuai", "glm-4.6");
        assert_eq!(
            d.provider_options.get("thinking"),
            Some(&json!({"type": "enabled", "clear_thinking": false}))
        );
    }

    #[test]
    fn test_gpt5_reasoning_effort() {
        let d = ModelDefaults::for_model("openai", "gpt-5");
        assert_eq!(
            d.provider_options.get("reasoningEffort"),
            Some(&json!("medium"))
        );
    }

    #[test]
    fn test_gpt5_pro_no_reasoning_effort() {
        let d = ModelDefaults::for_model("openai", "gpt-5-pro");
        assert_eq!(d.provider_options.get("reasoningEffort"), None);
    }

    #[test]
    fn test_gpt5_chat_no_reasoning_effort() {
        let d = ModelDefaults::for_model("openai", "gpt-5-chat");
        assert_eq!(d.provider_options.get("reasoningEffort"), None);
    }

    #[test]
    fn test_gpt5_dot_text_verbosity() {
        let d = ModelDefaults::for_model("openai", "gpt-5.1");
        assert_eq!(d.provider_options.get("textVerbosity"), Some(&json!("low")));
    }

    #[test]
    fn test_gpt5_dot_azure_no_text_verbosity() {
        let d = ModelDefaults::for_model("azure", "gpt-5.1");
        assert_eq!(d.provider_options.get("textVerbosity"), None);
    }

    #[test]
    fn test_gpt5_codex_no_text_verbosity() {
        let d = ModelDefaults::for_model("openai", "gpt-5-codex");
        assert_eq!(d.provider_options.get("textVerbosity"), None);
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
    fn test_apply_to_substitutes_session_id() {
        let d = ModelDefaults::for_model("openai", "gpt-4o");
        let mut config = json!({"model": "gpt-4o"});
        d.apply_to(&mut config, "sess-abc-123");

        assert_eq!(config["promptCacheKey"], json!("sess-abc-123"));
    }

    #[test]
    fn test_apply_to_user_prompt_cache_key_wins() {
        let d = ModelDefaults::for_model("openai", "gpt-4o");
        let mut config = json!({"model": "gpt-4o", "promptCacheKey": "custom-key"});
        d.apply_to(&mut config, "sess-abc-123");

        assert_eq!(config["promptCacheKey"], json!("custom-key"));
    }

    #[test]
    fn test_apply_to_provider_options_respect_user() {
        let d = ModelDefaults::for_model("openai", "gpt-4o");
        let mut config = json!({"model": "gpt-4o", "store": true});
        d.apply_to(&mut config, "session-1");

        // User's store: true wins over heuristic store: false
        assert_eq!(config["store"], json!(true));
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
        let d = ModelDefaults::for_model("anthropic", "claude-3-7-sonnet-20250219");
        let mut config = json!({
            "model": "claude-3-7-sonnet-20250219",
            "system": "You are a helpful assistant."
        });
        d.apply_to(&mut config, "session-1");

        // Should be transformed into an array of TextBlockParam with cache_control
        assert!(config["system"].is_array());
        let blocks = config["system"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "You are a helpful assistant.");
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn test_anthropic_system_caching_string_array() {
        let d = ModelDefaults::for_model("anthropic", "claude-3-7-sonnet-20250219");
        let mut config = json!({
            "model": "claude-3-7-sonnet-20250219",
            "system": ["Part 1", "Part 2"]
        });
        d.apply_to(&mut config, "session-1");

        // Each string should be wrapped as TextBlockParam with cache_control
        let blocks = config["system"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "Part 1");
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "Part 2");
        assert_eq!(blocks[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn test_anthropic_system_caching_existing_blocks() {
        let d = ModelDefaults::for_model("anthropic", "claude-3-7-sonnet-20250219");
        let mut config = json!({
            "model": "claude-3-7-sonnet-20250219",
            "system": [
                {
                    "type": "text",
                    "text": "Already a block"
                }
            ]
        });
        d.apply_to(&mut config, "session-1");

        // Should add cache_control to existing block
        let blocks = config["system"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "Already a block");
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
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
}
