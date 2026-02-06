use crate::tools::template::{anchor_pattern, regex_escape};
use llama_cpp_2::model::{AddBos, ChatTemplateResult, GrammarTriggerType, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use std::sync::Arc;

/// Build a grammar-constrained sampler from a ChatTemplateResult.
///
/// When a grammar is present, we use only `[grammar, greedy]` to match
/// the reference llama.cpp examples. Mixing temperature / top-p / top-k
/// with grammar sampling can corrupt the grammar state and trigger
/// assertion failures in llama-grammar.cpp.
pub(crate) fn build_tool_sampler(
    model: &Arc<LlamaModel>,
    result: &ChatTemplateResult,
    temperature: Option<f32>,
    seed: u32,
    top_p: Option<f32>,
    top_k: Option<u32>,
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
            // Grammar + greedy only — no temp/top_p/top_k
            return LlamaSampler::chain_simple([g, LlamaSampler::greedy()]);
        }
    }

    // No grammar or grammar creation failed — fall back to standard sampler
    build_standard_sampler(temperature, seed, top_p, top_k)
}

/// Build a standard sampler without grammar constraints.
pub(crate) fn build_standard_sampler(
    temperature: Option<f32>,
    seed: u32,
    top_p: Option<f32>,
    top_k: Option<u32>,
) -> LlamaSampler {
    let mut samplers = Vec::new();

    if let Some(temp) = temperature {
        if temp > 0.0 {
            samplers.push(LlamaSampler::temp(temp));
        }
    }
    if let Some(top_p) = top_p {
        samplers.push(LlamaSampler::top_p(top_p, 1));
    }
    if let Some(top_k) = top_k {
        samplers.push(LlamaSampler::top_k(top_k as i32));
    }

    let use_sampling = temperature.map_or(false, |t| t > 0.0) || top_p.is_some() || top_k.is_some();
    if use_sampling {
        samplers.push(LlamaSampler::dist(seed));
    } else {
        samplers.push(LlamaSampler::dist(seed));
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
