use crate::common_chat::ChatTemplateResult;
use crate::config::LlamaCppConfig;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use querymt::error::LLMError;
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

/// Build the sampler used for tool-capable generation.
pub(crate) fn build_tool_sampler(
    model: &Arc<LlamaModel>,
    result: &ChatTemplateResult,
    params: &SamplingParams,
) -> Result<LlamaSampler, LLMError> {
    #[cfg(feature = "common")]
    if let Some(tool_grammar) = &result.grammar {
        log::debug!(
            "build_tool_sampler: grammar lazy={}, root={}, triggers={:?}, grammar_len={}",
            tool_grammar.lazy,
            tool_grammar.root,
            tool_grammar
                .triggers
                .iter()
                .map(|t| &t.value)
                .collect::<Vec<_>>(),
            tool_grammar.grammar.len()
        );
        let grammar_sampler = if tool_grammar.lazy {
            let mut trigger_patterns = Vec::new();
            let mut trigger_tokens = Vec::new();

            for trigger in &tool_grammar.triggers {
                match model.str_to_token(&trigger.value, AddBos::Never) {
                    Ok(tokens) if tokens.len() == 1 => {
                        log::debug!(
                            "build_tool_sampler: trigger '{}' tokenized to single token {}",
                            trigger.value,
                            tokens[0]
                        );
                        trigger_tokens.push(tokens[0]);
                    }
                    Ok(tokens) => {
                        log::debug!(
                            "build_tool_sampler: trigger '{}' tokenized to {} tokens, using regex pattern instead",
                            trigger.value,
                            tokens.len()
                        );
                        trigger_patterns.push(regex_escape(&trigger.value));
                    }
                    Err(e) => {
                        log::debug!(
                            "build_tool_sampler: trigger '{}' failed to tokenize ({}), using regex pattern",
                            trigger.value,
                            e
                        );
                        trigger_patterns.push(regex_escape(&trigger.value));
                    }
                }
            }

            log::debug!(
                "build_tool_sampler: building lazy grammar with {} trigger_tokens, {} trigger_patterns",
                trigger_tokens.len(),
                trigger_patterns.len()
            );

            LlamaSampler::grammar_lazy_patterns(
                model,
                &tool_grammar.grammar,
                tool_grammar.root,
                &trigger_patterns,
                &trigger_tokens,
            )
        } else {
            log::debug!("build_tool_sampler: building strict (non-lazy) grammar");
            LlamaSampler::grammar(model, &tool_grammar.grammar, tool_grammar.root)
        }
        .map_err(|e| {
            LLMError::ProviderError(format!(
                "Failed to build tool grammar sampler: {e}. Grammar:\n{}",
                tool_grammar.grammar
            ))
        })?;

        log::debug!("build_tool_sampler: grammar sampler constructed successfully");

        return Ok(LlamaSampler::chain_simple([
            grammar_sampler,
            build_standard_sampler(params),
        ]));
    }

    #[cfg(feature = "common")]
    log::warn!(
        "build_tool_sampler: no tool grammar present in ChatTemplateResult — sampling unconstrained"
    );

    #[cfg(not(feature = "common"))]
    let _ = (model, result);

    Ok(build_standard_sampler(params))
}

fn regex_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '.' | '^' | '$' | '|' | '(' | ')' | '*' | '+' | '?' | '[' | ']' | '{' | '}' | '\\' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
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

    if let Some(top_k) = params.top_k {
        samplers.push(LlamaSampler::top_k(top_k as i32));
    }
    if let Some(top_p) = params.top_p {
        samplers.push(LlamaSampler::top_p(top_p, 1));
    }
    if let Some(min_p) = params.min_p {
        samplers.push(LlamaSampler::min_p(min_p, 1));
    }

    match params.temperature {
        Some(t) if t > 0.0 => {
            samplers.push(LlamaSampler::temp(t));
            samplers.push(LlamaSampler::dist(params.seed));
        }
        _ => samplers.push(LlamaSampler::greedy()),
    }

    LlamaSampler::chain_simple(samplers)
}

/// Conservative fallback used only when a model immediately emits EOG with the
/// configured sampler and no explicit sampling options were set.
pub(crate) fn build_fallback_sampler(seed: u32) -> LlamaSampler {
    LlamaSampler::chain_simple([
        LlamaSampler::top_k(40),
        LlamaSampler::top_p(0.95, 1),
        LlamaSampler::temp(0.8),
        LlamaSampler::dist(seed),
    ])
}
