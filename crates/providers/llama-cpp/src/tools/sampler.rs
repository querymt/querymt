use crate::config::LlamaCppConfig;
use crate::tools::template::{anchor_pattern, regex_escape};
use llama_cpp_2::model::{AddBos, ChatTemplateResult, GrammarTriggerType, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use std::sync::Arc;

/// Resolved sampling parameters for a single generation call.
///
/// Centralizes every knob that feeds the `LlamaSampler` chain so callers do
/// not have to thread 8+ `Option`s through `build_*_sampler` functions.
/// The `temperature` field wins over `cfg.temperature` to allow per-request
/// overrides (see `ChatProvider`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct SamplingParams {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub min_p: Option<f32>,
    pub repeat_penalty: Option<f32>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub penalty_last_n: Option<i32>,
    pub seed: u32,
}

impl SamplingParams {
    /// Build sampling params from config, allowing an optional per-request
    /// `temperature` override.
    pub(crate) fn from_config(cfg: &LlamaCppConfig, temperature: Option<f32>) -> Self {
        Self {
            temperature: temperature.or(cfg.temperature),
            top_p: cfg.top_p,
            top_k: cfg.top_k,
            min_p: cfg.min_p,
            repeat_penalty: cfg.repeat_penalty,
            presence_penalty: cfg.presence_penalty,
            frequency_penalty: cfg.frequency_penalty,
            penalty_last_n: cfg.penalty_last_n,
            seed: cfg.seed.unwrap_or(1234),
        }
    }

    /// Returns true when any sampling knob was explicitly configured.
    ///
    /// Used to decide whether the EOG-on-empty-output fallback sampler is
    /// allowed to kick in: if the user set *any* sampling parameter we must
    /// honour it rather than silently substituting defaults.
    pub(crate) fn is_explicit(&self) -> bool {
        self.temperature.map_or(false, |t| t > 0.0)
            || self.top_p.is_some()
            || self.top_k.is_some()
            || self.min_p.is_some()
            || self.repeat_penalty.is_some()
            || self.presence_penalty.is_some()
            || self.frequency_penalty.is_some()
    }
}

/// Build a grammar-constrained sampler from a ChatTemplateResult.
///
/// When a grammar is present, we use only `[grammar, greedy]` to match
/// the reference llama.cpp examples. Mixing temperature / top-p / top-k
/// with grammar sampling can corrupt the grammar state and trigger
/// assertion failures in llama-grammar.cpp.
pub(crate) fn build_tool_sampler(
    model: &Arc<LlamaModel>,
    result: &ChatTemplateResult,
    params: &SamplingParams,
) -> LlamaSampler {
    if let Some(ref grammar) = result.grammar {
        let grammar_sampler = if result.grammar_lazy {
            // Build lazy grammar sampler with triggers
            let mut trigger_patterns = Vec::new();
            let mut trigger_tokens = Vec::new();

            for trigger in &result.grammar_triggers {
                match trigger.trigger_type {
                    GrammarTriggerType::Token => {
                        if let Some(token) = trigger.token {
                            trigger_tokens.push(token);
                        }
                    }
                    GrammarTriggerType::Word => {
                        match model.str_to_token(&trigger.value, AddBos::Never) {
                            Ok(tokens) if tokens.len() == 1 => {
                                trigger_tokens.push(tokens[0]);
                            }
                            _ => {
                                trigger_patterns.push(regex_escape(&trigger.value));
                            }
                        }
                    }
                    GrammarTriggerType::Pattern => {
                        trigger_patterns.push(trigger.value.clone());
                    }
                    GrammarTriggerType::PatternFull => {
                        trigger_patterns.push(anchor_pattern(&trigger.value));
                    }
                }
            }

            LlamaSampler::grammar_lazy_patterns(
                model,
                grammar,
                "root",
                &trigger_patterns,
                &trigger_tokens,
            )
            .ok()
        } else {
            // Build strict grammar sampler
            LlamaSampler::grammar(model, grammar, "root").ok()
        };

        if let Some(g) = grammar_sampler {
            // Grammar + greedy only — no temp/top_p/top_k/min_p/penalties
            return LlamaSampler::chain_simple([g, LlamaSampler::greedy()]);
        }
    }

    // No grammar or grammar creation failed — fall back to standard sampler
    build_standard_sampler(params)
}

/// Build a standard sampler without grammar constraints.
pub(crate) fn build_standard_sampler(params: &SamplingParams) -> LlamaSampler {
    let mut samplers = Vec::new();

    // Penalties first — they modify logits before temperature/top-p sampling.
    if params.repeat_penalty.is_some()
        || params.presence_penalty.is_some()
        || params.frequency_penalty.is_some()
    {
        samplers.push(LlamaSampler::penalties(
            params.penalty_last_n.unwrap_or(64),
            params.repeat_penalty.unwrap_or(1.0),
            params.frequency_penalty.unwrap_or(0.0),
            params.presence_penalty.unwrap_or(0.0),
        ));
    }

    if let Some(temp) = params.temperature {
        if temp > 0.0 {
            samplers.push(LlamaSampler::temp(temp));
        }
    }
    if let Some(top_k) = params.top_k {
        samplers.push(LlamaSampler::top_k(top_k as i32));
    }
    if let Some(top_p) = params.top_p {
        samplers.push(LlamaSampler::top_p(top_p, 1));
    }
    if let Some(min_p) = params.min_p {
        samplers.push(LlamaSampler::min_p(min_p, 1));
    }

    samplers.push(LlamaSampler::dist(params.seed));
    if !params.is_explicit() {
        // Pure-default path: nudge toward greedy for deterministic output.
        samplers.push(LlamaSampler::greedy());
    }

    LlamaSampler::chain_simple(samplers)
}

/// Build a fallback sampler with default parameters.
pub(crate) fn build_fallback_sampler(seed: u32) -> LlamaSampler {
    LlamaSampler::chain_simple([
        LlamaSampler::temp(0.7),
        LlamaSampler::top_p(0.9, 1),
        LlamaSampler::dist(seed),
    ])
}
