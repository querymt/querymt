use crate::common_chat::ChatTemplateResult;
use crate::config::{GrammarBackend, LlamaCppConfig};
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;
use llama_cpp_2::token::data::LlamaTokenData;
use llama_cpp_2::token::data_array::LlamaTokenDataArray;
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

/// A token sampler whose grammar (if any) is enforced by rejection
/// sampling, the same way llama.cpp's `common_sampler_sample` does it:
///
/// 1. run the normal sampling chain on the raw logits and take its token;
/// 2. ask the grammar about that one token;
/// 3. only if it is rejected, mask the whole vocabulary with the grammar
///    and run the chain again.
///
/// Masking the vocabulary costs a grammar walk over every token (~250k for
/// recent vocabularies) and is what made tool-call generation CPU-bound
/// when the grammar ran first on every token. Models follow the grammar
/// almost always, so step 3 is rare. The resulting distribution equals the
/// grammar-first one: a token the grammar allows is drawn from the same
/// truncated distribution unless that distribution puts mass on forbidden
/// tokens, in which case the masked chain decides, as before. Only the RNG
/// stream differs (a rejected draw consumes one extra random number).
pub(crate) struct ConstrainedSampler {
    constraint: Option<Constraint>,
    chain: LlamaSampler,
    /// Reused candidate buffer (n_vocab entries).
    cur: LlamaTokenDataArray,
    /// Reused one-entry buffer for the single-token grammar check.
    probe: LlamaTokenDataArray,
    /// Whole-vocabulary masks computed so far (tests).
    #[cfg(test)]
    full_masks: u32,
}

enum Constraint {
    /// A llama.cpp sampler that masks logits (GBNF, or llguidance for
    /// structured output).
    Sampler(LlamaSampler),
    /// Lazy llguidance tool grammar, with what is needed to continue on
    /// GBNF if the matcher fails.
    #[cfg(feature = "common")]
    Llg(Box<LlgState>),
    /// No constraint left (llguidance failed and GBNF could not be built).
    #[cfg(feature = "common")]
    Open,
}

/// An llguidance tool grammar plus its GBNF equivalent (design D5).
#[cfg(feature = "common")]
struct LlgState {
    grammar: crate::tools::llg::LlgToolGrammar,
    gbnf: crate::common_chat::ToolGrammar,
    model: Arc<LlamaModel>,
    /// Every token accepted so far, replayed into GBNF on a switch.
    accepted: Vec<LlamaToken>,
}

#[cfg(feature = "common")]
impl LlgState {
    /// Build the GBNF sampler and replay the accepted tokens into it, which
    /// re-runs its lazy trigger.
    fn into_gbnf(self) -> Result<LlamaSampler, LLMError> {
        let mut sampler = build_gbnf_sampler(&self.model, &self.gbnf)?;
        for token in &self.accepted {
            sampler.accept(*token);
        }
        Ok(sampler)
    }
}

impl Constraint {
    fn allows(&mut self, token: LlamaToken, probe: &mut LlamaTokenDataArray) -> bool {
        match self {
            Self::Sampler(grammar) => {
                probe.data.clear();
                probe.data.push(LlamaTokenData::new(token, 0.0, 0.0));
                probe.selected = None;
                probe.sorted = false;
                grammar.apply(probe);
                probe.data.first().is_some_and(|d| d.logit().is_finite())
            }
            #[cfg(feature = "common")]
            Self::Llg(state) => {
                let allowed = state.grammar.allows(token);
                if self.recover() {
                    return self.allows(token, probe);
                }
                allowed
            }
            #[cfg(feature = "common")]
            Self::Open => true,
        }
    }

    /// If the llguidance matcher failed, continue on GBNF (or, should that
    /// fail too, unconstrained). Logged once; returns whether it switched.
    #[cfg(feature = "common")]
    fn recover(&mut self) -> bool {
        let Self::Llg(state) = self else {
            return false;
        };
        let Some(error) = state.grammar.failure().map(str::to_owned) else {
            return false;
        };
        let Self::Llg(state) = std::mem::replace(self, Self::Open) else {
            unreachable!("checked above");
        };
        crate::tools::llg::note_fallback(format_args!(
            "llguidance tool grammar failed ({error}); continuing with GBNF"
        ));
        match state.into_gbnf() {
            Ok(sampler) => *self = Self::Sampler(sampler),
            Err(error) => log::error!("GBNF fallback failed ({error}); output is unconstrained"),
        }
        true
    }

    fn apply(&mut self, cur: &mut LlamaTokenDataArray) {
        match self {
            Self::Sampler(grammar) => grammar.apply(cur),
            #[cfg(feature = "common")]
            Self::Llg(state) => {
                state.grammar.apply(cur);
                if self.recover() {
                    self.apply(cur);
                }
            }
            #[cfg(feature = "common")]
            Self::Open => {}
        }
    }

    fn accept(&mut self, token: LlamaToken) {
        match self {
            Self::Sampler(grammar) => grammar.accept(token),
            #[cfg(feature = "common")]
            Self::Llg(state) => {
                state.accepted.push(token);
                state.grammar.accept(token);
                self.recover();
            }
            #[cfg(feature = "common")]
            Self::Open => {}
        }
    }
}

/// Something that must see every token added to the context, including
/// tokens the provider forces (see `generation::force_text`).
pub(crate) trait AcceptToken {
    fn accept_token(&mut self, token: LlamaToken);
}

impl AcceptToken for LlamaSampler {
    fn accept_token(&mut self, token: LlamaToken) {
        self.accept(token);
    }
}

impl AcceptToken for ConstrainedSampler {
    fn accept_token(&mut self, token: LlamaToken) {
        self.accept(token);
    }
}

fn fill_candidates(cur: &mut LlamaTokenDataArray, logits: &[f32]) {
    cur.data.clear();
    cur.data.extend(
        logits
            .iter()
            .enumerate()
            .map(|(id, &logit)| LlamaTokenData::new(LlamaToken(id as i32), logit, 0.0)),
    );
    cur.selected = None;
    cur.sorted = false;
}

fn selected_or_argmax(cur: &LlamaTokenDataArray) -> LlamaToken {
    cur.selected_token().unwrap_or_else(|| {
        cur.data
            .iter()
            .max_by(|a, b| a.logit().total_cmp(&b.logit()))
            .map_or(LlamaToken(0), LlamaTokenData::id)
    })
}

impl ConstrainedSampler {
    fn new(constraint: Option<Constraint>, chain: LlamaSampler) -> Self {
        Self {
            constraint,
            chain,
            cur: LlamaTokenDataArray::new(Vec::new(), false),
            probe: LlamaTokenDataArray::new(Vec::with_capacity(1), false),
            #[cfg(test)]
            full_masks: 0,
        }
    }

    /// Sampler without a grammar.
    pub(crate) fn unconstrained(chain: LlamaSampler) -> Self {
        Self::new(None, chain)
    }

    /// Sample the next token from output `idx` of the last decode and
    /// accept it, like `LlamaSampler::sample`.
    pub(crate) fn sample(&mut self, ctx: &LlamaContext<'_>, idx: i32) -> LlamaToken {
        let logits = ctx.get_logits_ith(idx);
        let token = self.sample_logits(logits);
        self.accept(token);
        token
    }

    /// Choose a token from raw logits without accepting it.
    pub(crate) fn sample_logits(&mut self, logits: &[f32]) -> LlamaToken {
        #[cfg(test)]
        let full_masks = &mut self.full_masks;
        let Self {
            constraint,
            chain,
            cur,
            probe,
            ..
        } = self;
        fill_candidates(cur, logits);
        chain.apply(cur);
        let token = selected_or_argmax(cur);
        let Some(constraint) = constraint else {
            return token;
        };
        if constraint.allows(token, probe) {
            return token;
        }
        #[cfg(test)]
        {
            *full_masks += 1;
        }
        fill_candidates(cur, logits);
        constraint.apply(cur);
        chain.apply(cur);
        selected_or_argmax(cur)
    }

    /// Record a token that was added to the context.
    pub(crate) fn accept(&mut self, token: LlamaToken) {
        if let Some(constraint) = self.constraint.as_mut() {
            constraint.accept(token);
        }
        self.chain.accept(token);
    }

    /// Apply only the grammar to `candidates` (tests and benchmarks).
    #[cfg(test)]
    pub(crate) fn mask(&mut self, candidates: &mut LlamaTokenDataArray) {
        if let Some(constraint) = self.constraint.as_mut() {
            constraint.apply(candidates);
        }
    }

    /// Grammar tier in use, `None` when not llguidance (tests).
    #[cfg(all(test, feature = "common"))]
    pub(crate) fn llg_tier(&self) -> Option<crate::tools::llg::Tier> {
        match self.constraint.as_ref() {
            Some(Constraint::Llg(state)) => Some(state.grammar.tier()),
            _ => None,
        }
    }

    /// Simulate an llguidance matcher error (tests).
    #[cfg(all(test, feature = "common"))]
    pub(crate) fn inject_llg_error(&mut self) {
        if let Some(Constraint::Llg(state)) = self.constraint.as_mut() {
            state.grammar.inject_error();
        }
    }

    /// Whole-vocabulary masks computed by `sample_logits` (tests).
    #[cfg(test)]
    pub(crate) fn full_masks(&self) -> u32 {
        self.full_masks
    }

    /// Whether the llguidance backend is in use (tests).
    #[cfg(test)]
    pub(crate) fn uses_llguidance(&self) -> bool {
        #[cfg(feature = "common")]
        {
            matches!(self.constraint, Some(Constraint::Llg(_)))
        }
        #[cfg(not(feature = "common"))]
        {
            false
        }
    }

    /// Whether the grammar allows `token` next (tests and benchmarks).
    #[cfg(test)]
    pub(crate) fn allows(&mut self, token: LlamaToken) -> bool {
        let Self {
            constraint, probe, ..
        } = self;
        constraint.as_mut().is_none_or(|c| c.allows(token, probe))
    }
}

/// Build the sampler used for tool-capable generation.
pub(crate) fn build_tool_sampler(
    model: &Arc<LlamaModel>,
    result: &ChatTemplateResult,
    params: &SamplingParams,
    backend: GrammarBackend,
) -> Result<ConstrainedSampler, LLMError> {
    let chain = build_standard_sampler(model, params);
    let Some(tool_grammar) = &result.grammar else {
        log::warn!(
            "build_tool_sampler: no tool grammar present in ChatTemplateResult — sampling unconstrained"
        );
        return Ok(ConstrainedSampler::unconstrained(chain));
    };

    #[cfg(feature = "common")]
    if backend == GrammarBackend::Llguidance {
        match crate::tools::llg::LlgToolGrammar::build(model, tool_grammar, &result.tools) {
            Ok(Some(grammar)) => {
                log::debug!(
                    "build_tool_sampler: using llguidance tool grammar ({:?} tier)",
                    grammar.tier()
                );
                let state = LlgState {
                    grammar,
                    gbnf: tool_grammar.clone(),
                    model: Arc::clone(model),
                    accepted: Vec::new(),
                };
                return Ok(ConstrainedSampler::new(
                    Some(Constraint::Llg(Box::new(state))),
                    chain,
                ));
            }
            Ok(None) => log::debug!(
                "build_tool_sampler: no llguidance port for {:?}; using GBNF",
                tool_grammar.format
            ),
            Err(error) => crate::tools::llg::note_fallback(format_args!(
                "llguidance tool grammar unavailable ({error}); using GBNF"
            )),
        }
    }
    #[cfg(not(feature = "common"))]
    let _ = backend;

    let grammar = build_gbnf_sampler(model, tool_grammar)?;
    Ok(ConstrainedSampler::new(
        Some(Constraint::Sampler(grammar)),
        chain,
    ))
}

fn build_gbnf_sampler(
    model: &Arc<LlamaModel>,
    tool_grammar: &crate::common_chat::ToolGrammar,
) -> Result<LlamaSampler, LLMError> {
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
    if tool_grammar.lazy {
        let mut trigger_patterns = Vec::new();
        let mut trigger_tokens = Vec::new();
        for trigger in &tool_grammar.triggers {
            let tokens = model.vocab().tokenize(trigger.value.as_bytes(), false, true);
            match tokens.as_slice() {
                [token] => trigger_tokens.push(*token),
                tokens => {
                    log::debug!(
                        "build_tool_sampler: trigger '{}' tokenized to {} tokens, using regex pattern instead",
                        trigger.value,
                        tokens.len()
                    );
                    trigger_patterns.push(regex_escape(&trigger.value));
                }
            }
        }
        LlamaSampler::grammar_lazy_patterns(
            model,
            &tool_grammar.grammar,
            tool_grammar.root,
            &trigger_patterns,
            &trigger_tokens,
        )
    } else {
        LlamaSampler::grammar(model, &tool_grammar.grammar, tool_grammar.root)
    }
    .map_err(|e| {
        LLMError::ProviderError(format!(
            "Failed to build tool grammar sampler: {e}. Grammar:\n{}",
            tool_grammar.grammar
        ))
    })
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
pub(crate) fn build_standard_sampler(model: &LlamaModel, params: &SamplingParams) -> LlamaSampler {
    let mut samplers = Vec::new();

    // Penalties first — they modify logits before temperature/top-p sampling.
    if params.repeat_penalty.is_some()
        || params.presence_penalty.is_some()
        || params.frequency_penalty.is_some()
    {
        samplers.push(LlamaSampler::penalties(
            model.n_vocab(),
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

#[cfg(feature = "llguidance")]
pub(crate) fn build_structured_sampler(
    model: &Arc<LlamaModel>,
    schema: &serde_json::Value,
    params: &SamplingParams,
) -> Result<ConstrainedSampler, LLMError> {
    use llguidance::{Matcher, ParserFactory, api::TopLevelGrammar};
    use std::sync::Arc as StdArc;
    use toktrie::{ApproximateTokEnv, TokRxInfo, TokTrie};

    let n_vocab = model.n_vocab() as u32;
    let vocab = model.vocab();
    let eos = vocab.eos().0 as u32;
    let mut eog_tokens = vec![eos];
    let mut words = Vec::with_capacity(n_vocab as usize);
    for id in 0..n_vocab as i32 {
        let token = llama_cpp_2::token::LlamaToken(id);
        if vocab.is_eog(token) && id as u32 != eos {
            eog_tokens.push(id as u32);
        }
        let mut bytes = vocab.token_to_piece(token, false, None);
        if bytes.is_empty() {
            bytes = vocab.token_to_piece(token, true, None);
            if !bytes.is_empty() {
                bytes.insert(0, 0xff);
            }
        }
        words.push(bytes);
    }
    let trie = TokTrie::from(&TokRxInfo::new(n_vocab, eos), &words).with_eos_tokens(&eog_tokens);
    let tok_env: toktrie::TokEnv = StdArc::new(ApproximateTokEnv::new(trie));
    let mut factory = ParserFactory::new_simple(&tok_env)
        .map_err(|e| LLMError::ProviderError(format!("Failed to initialize llguidance: {e}")))?;
    factory.quiet();
    let parser = factory.create_parser(TopLevelGrammar::from_json_schema(schema.clone()));
    let matcher = Matcher::new(parser);
    Ok(ConstrainedSampler::new(
        Some(Constraint::Sampler(LlamaSampler::from(matcher))),
        build_standard_sampler(model, params),
    ))
}

#[cfg(not(feature = "llguidance"))]
pub(crate) fn build_structured_sampler(
    model: &Arc<LlamaModel>,
    schema: &serde_json::Value,
    params: &SamplingParams,
) -> Result<ConstrainedSampler, LLMError> {
    let grammar = llama_cpp_2::json_schema_to_grammar(&schema.to_string())
        .map_err(|e| LLMError::ProviderError(format!("Failed to compile JSON schema: {e}")))?;
    let constraint = LlamaSampler::grammar(model, &grammar, "root")
        .map_err(|e| LLMError::ProviderError(format!("Failed to build JSON grammar: {e}")))?;
    Ok(ConstrainedSampler::new(
        Some(Constraint::Sampler(constraint)),
        build_standard_sampler(model, params),
    ))
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

#[cfg(all(test, feature = "common"))]
mod grammar_tests {
    //! Run the tool grammar through llama.cpp itself, using a vocab-only GGUF
    //! (tokenizer only, no weights, ~6 MB) so no real model is needed.

    use super::*;
    use crate::chat_format::ToolFormat;
    use crate::common_chat::ReasoningFormat;
    use crate::test_vocab::{self, Vocab};
    use llama_cpp_2::token::LlamaToken;
    use llama_cpp_2::token::data::LlamaTokenData;
    use llama_cpp_2::token::data_array::LlamaTokenDataArray;
    use querymt::chat::{FunctionTool, Tool};

    /// `write_file` tool with string `path` and `content` parameters.
    fn write_file_tool() -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: "write_file".to_string(),
                description: "Write a file".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }),
                strict: None,
            },
        }
    }

    /// Wrap a tool grammar in an otherwise empty template result.
    fn template_result(grammar: crate::common_chat::ToolGrammar) -> ChatTemplateResult {
        ChatTemplateResult {
            prompt: String::new(),
            grammar: Some(grammar),
            preserved_tokens: Vec::new(),
            additional_stops: Vec::new(),
            starts_in_thinking: false,
            reasoning_format: ReasoningFormat::ThinkTags,
            tools: vec![write_file_tool()],
        }
    }

    /// Feed `prefix` through the sampler, then report which of `probe`'s
    /// tokens the grammar still allows next.
    fn allowed_after(
        model: &Arc<LlamaModel>,
        grammar: crate::common_chat::ToolGrammar,
        prefix: &str,
        probe: &str,
    ) -> Vec<(LlamaToken, bool)> {
        allowed_after_with(model, grammar, prefix, probe, GrammarBackend::Gbnf)
    }

    fn sampler_after(
        model: &Arc<LlamaModel>,
        grammar: crate::common_chat::ToolGrammar,
        prefix: &str,
        backend: GrammarBackend,
    ) -> ConstrainedSampler {
        let result = template_result(grammar);
        let cfg: LlamaCppConfig =
            serde_json::from_value(serde_json::json!({ "model": "vocab-only" })).unwrap();
        let params = SamplingParams::from_config(&cfg, None);
        let mut sampler = build_tool_sampler(model, &result, &params, backend).expect("sampler");
        for token in model.vocab().tokenize(prefix.as_bytes(), false, true) {
            sampler.accept(token);
        }
        sampler
    }

    fn full_vocab(model: &LlamaModel) -> LlamaTokenDataArray {
        LlamaTokenDataArray::from_iter(
            (0..model.n_vocab()).map(|id| LlamaTokenData::new(LlamaToken(id), 0.0, 0.0)),
            false,
        )
    }

    fn allowed_after_with(
        model: &Arc<LlamaModel>,
        grammar: crate::common_chat::ToolGrammar,
        prefix: &str,
        probe: &str,
        backend: GrammarBackend,
    ) -> Vec<(LlamaToken, bool)> {
        let mut sampler = sampler_after(model, grammar, prefix, backend);
        let mut candidates = full_vocab(model);
        sampler.mask(&mut candidates);
        let probes = model.vocab().tokenize(probe.as_bytes(), false, true);
        probes
            .into_iter()
            .map(|token| {
                let logit = candidates.data[token.0 as usize].logit();
                (token, logit.is_finite())
            })
            .collect()
    }

    #[test]
    fn value_accepts_less_than_and_markup() {
        let Some(model) = vocab_model() else { return };

        let prefix = "<tool_call>\n<function=write_file>\n<parameter=path>\na.py\n</parameter>\n<parameter=content>\nif row[j]";
        let probe = " <";
        let tool = write_file_tool();

        // Fixed grammar: ` <` must stay available mid-value.
        let fixed = ToolFormat::QwenFunction
            .grammar(std::slice::from_ref(&tool))
            .unwrap();
        let allowed = allowed_after(&model, fixed.clone(), prefix, probe);
        assert!(!allowed.is_empty(), "probe tokenized to nothing");
        assert!(
            allowed.iter().all(|(_, ok)| *ok),
            "`<` was masked by the fixed grammar: {allowed:?}"
        );

        // Negative control: the old `[^<]*` rule masks it. If this ever stops
        // failing, the test is no longer proving anything.
        let mut old = fixed;
        old.grammar = old
            .grammar
            .lines()
            .filter(|line| !line.starts_with("value-text"))
            .chain(std::iter::once("value-text ::= [^<]*"))
            .collect::<Vec<_>>()
            .join("\n");
        let allowed = allowed_after(&model, old, prefix, probe);
        assert!(
            allowed.iter().any(|(_, ok)| !*ok),
            "old grammar unexpectedly allowed `<`: {allowed:?}"
        );
    }

    #[test]
    fn grammar_still_closes_the_parameter() {
        let Some(model) = vocab_model() else { return };
        let grammar = ToolFormat::QwenFunction
            .grammar(&[write_file_tool()])
            .unwrap();
        // After `</parameter` only `>` may follow: the terminator cannot be
        // broken into something else, and `</parameter>` itself is accepted.
        let prefix = "<tool_call>\n<function=write_file>\n<parameter=path>\na.py\n</parameter>\n<parameter=content>\nx\n</parameter";
        let allowed = allowed_after(&model, grammar, prefix, ">");
        assert!(allowed.iter().all(|(_, ok)| *ok), "{allowed:?}");
    }

    fn vocab_model() -> Option<Arc<LlamaModel>> {
        test_vocab::vocab_model(Vocab::Qwen35)
    }

    const CONTENT_PREFIX: &str = "<tool_call>\n<function=write_file>\n<parameter=path>\na.py\n</parameter>\n<parameter=content>\nif row[j]";

    /// The llguidance port must keep the `<` fix and still close parameters.
    #[test]
    fn multi_token_trigger_activates_the_grammar() {
        let Some(model) = vocab_model() else { return };
        let grammar = ToolFormat::QwenFunction
            .grammar(&[write_file_tool()])
            .unwrap();
        let allowed = allowed_after_with(
            &model,
            grammar.clone(),
            CONTENT_PREFIX,
            " <",
            GrammarBackend::Llguidance,
        );
        assert!(allowed.iter().all(|(_, ok)| *ok), "{allowed:?}");
        let prefix = "<tool_call>\n<function=write_file>\n<parameter=path>\na.py\n</parameter>\n<parameter=content>\nx\n</parameter";
        let allowed = allowed_after_with(
            &model,
            grammar.clone(),
            prefix,
            ">",
            GrammarBackend::Llguidance,
        );
        assert!(allowed.iter().all(|(_, ok)| *ok), "{allowed:?}");

        // Over the whole vocabulary both backends agree on what may follow
        // `<parameter=content>\n...` up to tokens that differ only by how
        // the engines treat special/control pieces.
        let mut gbnf = sampler_after(
            &model,
            grammar.clone(),
            CONTENT_PREFIX,
            GrammarBackend::Gbnf,
        );
        let mut llg = sampler_after(&model, grammar, CONTENT_PREFIX, GrammarBackend::Llguidance);
        if !llg.uses_llguidance() {
            let tools = [write_file_tool()];
            let g = ToolFormat::QwenFunction.grammar(&tools).unwrap();
            let r =
                crate::tools::llg::LlgToolGrammar::build(&model, &g, &tools).map(|o| o.is_some());
            panic!(
                "llguidance backend fell back to GBNF: {r:?}; trigger tokens {:?}",
                model.vocab().tokenize(b"<tool_call>", false, true)
            );
        }
        let mut a = full_vocab(&model);
        let mut b = full_vocab(&model);
        gbnf.mask(&mut a);
        llg.mask(&mut b);
        let vocab = model.vocab();
        let mut differ = Vec::new();
        for (x, y) in a.data.iter().zip(&b.data) {
            if x.logit().is_finite() != y.logit().is_finite() {
                let piece = vocab.token_to_piece(x.id(), false, None);
                // Byte-fallback tokens that are not valid UTF-8 on their own
                // (and NUL): llama.cpp rejects them outright, llguidance
                // tracks partial UTF-8. Not a difference in the language.
                if !piece.is_empty() && !piece.contains(&0) && std::str::from_utf8(&piece).is_ok() {
                    differ.push((x.id(), String::from_utf8_lossy(&piece).into_owned()));
                }
            }
        }
        assert!(
            differ.is_empty(),
            "{} tokens differ: {:?}",
            differ.len(),
            &differ[..differ.len().min(20)]
        );
    }

    /// Rejection sampling picks exactly what grammar-first greedy would.
    #[test]
    fn rejected_favourite_is_resampled_from_allowed_tokens() {
        let Some(model) = vocab_model() else { return };
        let grammar = ToolFormat::QwenFunction
            .grammar(&[write_file_tool()])
            .unwrap();
        // Right after the trigger only `\n` (then `<function=`) is legal.
        // Make a forbidden token the favourite and `\n` the runner-up.
        let prefix = "<tool_call>";
        let nl = model.vocab().tokenize(b"\n", false, true);
        assert_eq!(nl.len(), 1);
        let gt = nl[0];
        let bad = model.vocab().tokenize(b"s", false, true)[0];
        let mut logits = vec![0.0f32; model.n_vocab() as usize];
        logits[bad.0 as usize] = 10.0;
        logits[gt.0 as usize] = 5.0;
        for backend in [GrammarBackend::Gbnf, GrammarBackend::Llguidance] {
            let mut sampler = sampler_after(&model, grammar.clone(), prefix, backend);
            assert!(!sampler.allows(bad), "{backend:?}");
            assert_eq!(sampler.sample_logits(&logits), gt, "{backend:?}");
        }
        // Unconstrained position: the favourite wins without a full mask.
        let mut sampler = sampler_after(&model, grammar, CONTENT_PREFIX, GrammarBackend::Gbnf);
        assert_eq!(sampler.sample_logits(&logits), bad);
    }

    /// Sampler for a config JSON, resolving the engine the way the
    /// generation paths do.
    fn sampler_for_config(
        model: &Arc<LlamaModel>,
        config: serde_json::Value,
    ) -> ConstrainedSampler {
        let mut config = config;
        config["model"] = "vocab-only".into();
        let cfg: LlamaCppConfig = serde_json::from_value(config).unwrap();
        let grammar = ToolFormat::QwenFunction
            .grammar(&[write_file_tool()])
            .unwrap();
        build_tool_sampler(
            model,
            &template_result(grammar),
            &SamplingParams::from_config(&cfg, None),
            cfg.grammar_backend.unwrap_or_default(),
        )
        .expect("sampler")
    }

    #[test]
    fn grammar_backend_defaults_to_llguidance() {
        let Some(model) = vocab_model() else { return };
        let sampler = sampler_for_config(&model, serde_json::json!({}));
        assert!(sampler.uses_llguidance());
    }

    #[test]
    fn grammar_backend_gbnf_selects_gbnf() {
        let Some(model) = vocab_model() else { return };
        let sampler = sampler_for_config(&model, serde_json::json!({"grammar_backend": "gbnf"}));
        assert!(!sampler.uses_llguidance());
    }

    /// Tokenizer of a real model that uses `format` (design D7).
    fn vocab_for(format: ToolFormat) -> Vocab {
        match format {
            ToolFormat::Qwen3 | ToolFormat::QwenFunction | ToolFormat::Ministral3 => Vocab::Qwen35,
            ToolFormat::Gemma4 => Vocab::Gemma4,
            ToolFormat::Lfm2 => Vocab::Lfm2,
            ToolFormat::FunctionGemma => Vocab::FunctionGemma,
        }
    }

    const ALL_FORMATS: [ToolFormat; 6] = [
        ToolFormat::Qwen3,
        ToolFormat::QwenFunction,
        ToolFormat::Ministral3,
        ToolFormat::Lfm2,
        ToolFormat::FunctionGemma,
        ToolFormat::Gemma4,
    ];

    /// Llguidance sampler for `format`'s grammar over `tools`.
    fn llg_sampler(
        model: &Arc<LlamaModel>,
        format: ToolFormat,
        tools: &[Tool],
    ) -> ConstrainedSampler {
        let mut result = template_result(format.grammar(tools).unwrap());
        result.tools = tools.to_vec();
        let cfg: LlamaCppConfig =
            serde_json::from_value(serde_json::json!({ "model": "vocab-only" })).unwrap();
        build_tool_sampler(
            model,
            &result,
            &SamplingParams::from_config(&cfg, None),
            GrammarBackend::Llguidance,
        )
        .expect("sampler")
    }

    #[test]
    fn every_detected_format_compiles_under_llguidance() {
        use crate::tools::llg::Tier;
        let tools = [write_file_tool()];
        for format in ALL_FORMATS {
            let Some(model) = test_vocab::vocab_model(vocab_for(format)) else {
                continue;
            };
            let before = fallbacks();
            let sampler = llg_sampler(&model, format, &tools);
            assert_eq!(sampler.llg_tier(), Some(Tier::Schema), "{format:?}");
            assert_eq!(fallbacks() - before, 0, "{format:?}: a fallback was logged");
        }
    }

    /// Sampler for `prefix` on the generic llguidance tier.
    fn generic_llg_sampler(
        model: &Arc<LlamaModel>,
        format: ToolFormat,
        prefix: &str,
    ) -> ConstrainedSampler {
        use crate::tools::llg::{Tier, test_hooks::FORCE_GENERIC};
        FORCE_GENERIC.with(|f| f.set(true));
        let sampler = sampler_after(
            model,
            format.grammar(&[write_file_tool()]).unwrap(),
            prefix,
            GrammarBackend::Llguidance,
        );
        FORCE_GENERIC.with(|f| f.set(false));
        assert_eq!(sampler.llg_tier(), Some(Tier::Generic), "{format:?}");
        sampler
    }

    /// Tokens the two samplers disagree on, as `(id, text)`. Two classes
    /// are left out, neither a difference in the grammar's language:
    /// byte tokens that are not UTF-8 on their own (llama.cpp rejects them
    /// outright, llguidance tracks partial UTF-8 across tokens), and
    /// control tokens such as `<|im_start|>` (GBNF matches their spelling
    /// inside text, llguidance keeps special tokens out of text lexemes;
    /// they render as an empty piece without `special`).
    fn differing_tokens(
        model: &LlamaModel,
        a: &mut ConstrainedSampler,
        b: &mut ConstrainedSampler,
    ) -> Vec<(LlamaToken, String)> {
        let (mut x, mut y) = (full_vocab(model), full_vocab(model));
        a.mask(&mut x);
        b.mask(&mut y);
        let vocab = model.vocab();
        x.data
            .iter()
            .zip(&y.data)
            .filter(|(p, q)| p.logit().is_finite() != q.logit().is_finite())
            .filter_map(|(p, _)| {
                let piece = vocab.token_to_piece(p.id(), false, None);
                (!piece.is_empty() && !piece.contains(&0) && std::str::from_utf8(&piece).is_ok())
                    .then(|| (p.id(), String::from_utf8_lossy(&piece).into_owned()))
            })
            .collect()
    }

    /// Positions (text fed after the start of the call) where GBNF and the
    /// generic grammar are compared, per format: right after the trigger,
    /// inside the call head, and inside a value.
    const PARITY: [(ToolFormat, &[&str]); 6] = [
        (
            ToolFormat::Qwen3,
            &[
                "<tool_call>",
                "<tool_call>{\"name\": \"write_file\", \"arguments\": {\"path\": \"a.py\", \"content\": \"x < y",
                "<tool_call>{\"name\": \"write_file\", \"arguments\": {\"path\": \"a.py\", \"content\": \"x\"}",
            ],
        ),
        (
            ToolFormat::QwenFunction,
            &[
                "<tool_call>",
                "<tool_call>\n<function=write_file>\n",
                CONTENT_PREFIX,
                "<tool_call>\n<function=write_file>\n<parameter=path>\na.py\n</parameter>\n<parameter=content>\nx\n</parameter",
            ],
        ),
        (
            ToolFormat::Ministral3,
            &[
                "[TOOL_CALLS]",
                "[TOOL_CALLS]write_file[ARGS]",
                "[TOOL_CALLS]write_file[ARGS]{\"path\": \"a\", \"content\": \"x < y",
            ],
        ),
        (
            ToolFormat::Lfm2,
            &[
                "<|tool_call_start|>",
                "<|tool_call_start|>[write_file(",
                "<|tool_call_start|>[write_file(path=\"a\", content=\"x < y",
            ],
        ),
        (
            ToolFormat::FunctionGemma,
            &[
                "<start_function_call>",
                "<start_function_call>call:write_file{",
                "<start_function_call>call:write_file{path:<escape>a<escape>,content:<escape>x < y",
            ],
        ),
        (
            ToolFormat::Gemma4,
            &[
                "<|tool_call>",
                "<|tool_call>call:write_file{",
                "<|tool_call>call:write_file{path:<|\"|>a<|\"|>,content:<|\"|>x < y",
            ],
        ),
    ];

    #[test]
    fn generic_grammar_matches_gbnf_language() {
        for (format, positions) in PARITY {
            let Some(model) = test_vocab::vocab_model(vocab_for(format)) else {
                continue;
            };
            let grammar = format.grammar(&[write_file_tool()]).unwrap();
            for prefix in positions {
                let mut gbnf = sampler_after(&model, grammar.clone(), prefix, GrammarBackend::Gbnf);
                let mut llg = generic_llg_sampler(&model, format, prefix);
                let differ = differing_tokens(&model, &mut gbnf, &mut llg);
                assert!(
                    differ.is_empty(),
                    "{format:?} after {prefix:?}: {} tokens differ: {:?}",
                    differ.len(),
                    &differ[..differ.len().min(20)]
                );
            }
        }
    }

    #[test]
    fn every_format_is_checked_on_its_models_tokenizer() {
        for format in ALL_FORMATS {
            let positions = PARITY
                .iter()
                .find(|(f, _)| *f == format)
                .map_or(&[][..], |(_, p)| p);
            assert!(
                positions.len() >= 3,
                "{format:?} has no real-tokenizer check"
            );
            // The checks run on the tokenizer of a model that uses the format.
            let expected = match format {
                ToolFormat::Qwen3 | ToolFormat::QwenFunction | ToolFormat::Ministral3 => {
                    Vocab::Qwen35
                }
                ToolFormat::Gemma4 => Vocab::Gemma4,
                ToolFormat::Lfm2 => Vocab::Lfm2,
                ToolFormat::FunctionGemma => Vocab::FunctionGemma,
            };
            assert_eq!(vocab_for(format), expected, "{format:?}");
        }
        assert_eq!(PARITY.len(), ALL_FORMATS.len());
    }

    const BACKENDS: [GrammarBackend; 2] = [GrammarBackend::Gbnf, GrammarBackend::Llguidance];

    fn token(model: &LlamaModel, text: &str) -> LlamaToken {
        let tokens = model.vocab().tokenize(text.as_bytes(), false, true);
        assert_eq!(
            tokens.len(),
            1,
            "{text:?} is not a single token: {tokens:?}"
        );
        tokens[0]
    }

    #[test]
    fn special_token_trigger_activates_the_grammar() {
        for (format, trigger) in [
            (ToolFormat::Gemma4, "<|tool_call>"),
            (ToolFormat::Lfm2, "<|tool_call_start|>"),
            (ToolFormat::FunctionGemma, "<start_function_call>"),
        ] {
            let Some(model) = test_vocab::vocab_model(vocab_for(format)) else {
                continue;
            };
            let grammar = format.grammar(&[write_file_tool()]).unwrap();
            // Every format starts its call with a fixed character, so a
            // letter that is not that character must be rejected.
            let (bad, good) = (token(&model, "x"), token(&model, "c"));
            let (bad, good) = if format == ToolFormat::Lfm2 {
                (bad, token(&model, "["))
            } else {
                (bad, good)
            };
            token(&model, trigger);
            for backend in BACKENDS {
                let mut before = sampler_after(&model, grammar.clone(), "Hello ", backend);
                assert!(
                    before.allows(bad),
                    "{format:?} {backend:?}: active before the trigger"
                );
                let mut after = sampler_after(&model, grammar.clone(), trigger, backend);
                assert!(
                    !after.allows(bad),
                    "{format:?} {backend:?}: not active after {trigger}"
                );
                assert!(after.allows(good), "{format:?} {backend:?}");
            }
        }
    }

    #[test]
    fn special_token_terminator_closes_the_value() {
        for (format, prefix, terminator) in [
            (
                ToolFormat::Gemma4,
                "<|tool_call>call:write_file{path:<|\"|>a.py",
                "<|\"|>",
            ),
            (
                ToolFormat::FunctionGemma,
                "<start_function_call>call:write_file{path:<escape>a.py",
                "<escape>",
            ),
        ] {
            let Some(model) = test_vocab::vocab_model(vocab_for(format)) else {
                continue;
            };
            let grammar = format.grammar(&[write_file_tool()]).unwrap();
            let (close, comma, letter) = (
                token(&model, terminator),
                token(&model, ","),
                token(&model, "x"),
            );
            for backend in BACKENDS {
                let mut sampler = sampler_after(&model, grammar.clone(), prefix, backend);
                assert!(sampler.allows(letter), "{format:?} {backend:?}: value text");
                assert!(
                    sampler.allows(close),
                    "{format:?} {backend:?}: terminator rejected"
                );
                sampler.accept(close);
                assert!(
                    sampler.allows(comma),
                    "{format:?} {backend:?}: value not closed"
                );
                assert!(
                    !sampler.allows(letter),
                    "{format:?} {backend:?}: text after the value"
                );
            }
        }
    }

    #[test]
    fn text_before_the_trigger_is_unconstrained() {
        let prose = "Some prose with < and { braces, call:foo and the word function ";
        for format in ALL_FORMATS {
            let Some(model) = test_vocab::vocab_model(vocab_for(format)) else {
                continue;
            };
            let grammar = format.grammar(&[write_file_tool()]).unwrap();
            for backend in BACKENDS {
                let mut sampler = sampler_after(&model, grammar.clone(), prose, backend);
                let mut candidates = full_vocab(&model);
                sampler.mask(&mut candidates);
                let masked = candidates
                    .data
                    .iter()
                    .filter(|d| !d.logit().is_finite())
                    .count();
                assert_eq!(masked, 0, "{format:?} {backend:?}: {masked} tokens masked");
            }
        }
    }

    /// One complete call per format, starting with the trigger.
    const COMPLETE_CALLS: [(ToolFormat, &str); 6] = [
        (
            ToolFormat::Qwen3,
            "<tool_call>\n{\"name\": \"write_file\", \"arguments\": {\"path\": \"a.py\", \"content\": \"x\"}}\n</tool_call>",
        ),
        (
            ToolFormat::QwenFunction,
            "<tool_call>\n<function=write_file>\n<parameter=path>\na.py\n</parameter>\n<parameter=content>\nx\n</parameter>\n</function>\n</tool_call>",
        ),
        (
            ToolFormat::Ministral3,
            "[TOOL_CALLS]write_file[ARGS]{\"path\": \"a\", \"content\": \"x\"}",
        ),
        (
            ToolFormat::Lfm2,
            "<|tool_call_start|>[write_file(path=\"a\", content=\"x\")]<|tool_call_end|>",
        ),
        (
            ToolFormat::FunctionGemma,
            "<start_function_call>call:write_file{path:<escape>a<escape>,content:<escape>x<escape>}<end_function_call>",
        ),
        (
            ToolFormat::Gemma4,
            "<|tool_call>call:write_file{path:<|\"|>a<|\"|>,content:<|\"|>x<|\"|>}<tool_call|>",
        ),
    ];

    /// Feed `text` token by token to a fresh sampler. `Err` carries the
    /// index and text of the first token the grammar rejected.
    fn replay(
        model: &Arc<LlamaModel>,
        format: ToolFormat,
        backend: GrammarBackend,
        text: &str,
    ) -> Result<ConstrainedSampler, (usize, String)> {
        let grammar = format.grammar(&[write_file_tool()]).unwrap();
        let mut sampler = sampler_after(model, grammar, "", backend);
        for (i, token) in model
            .vocab()
            .tokenize(text.as_bytes(), false, true)
            .into_iter()
            .enumerate()
        {
            if !sampler.allows(token) {
                let piece = model.vocab().token_to_piece(token, true, None);
                return Err((i, String::from_utf8_lossy(&piece).into_owned()));
            }
            sampler.accept(token);
        }
        Ok(sampler)
    }

    /// Whether some end-of-generation token (`<|im_end|>`, ...) is allowed.
    fn end_of_turn_allowed(model: &LlamaModel, sampler: &mut ConstrainedSampler) -> bool {
        (0..model.n_vocab())
            .map(LlamaToken)
            .filter(|token| model.vocab().is_eog(*token))
            .any(|token| sampler.allows(token))
    }

    #[test]
    fn qwen3_json_tool_name_must_be_a_json_string() {
        let Some(model) = test_vocab::vocab_model(Vocab::Qwen35) else {
            return;
        };
        let tools = [write_file_tool()];
        let (_, call) = COMPLETE_CALLS[0];
        // The provider parses what the grammar lets through.
        let parsed = crate::chat_format::parse_assistant_format_with_tools(
            call,
            ReasoningFormat::ThinkTags,
            false,
            &tools,
        );
        assert_eq!(
            parsed.tool_calls.expect("parses")[0].function.name,
            "write_file"
        );

        let unquoted = call.replace("\"name\": \"write_file\"", "\"name\": write_file");
        assert_ne!(unquoted, call);
        for backend in BACKENDS {
            replay(&model, ToolFormat::Qwen3, backend, call)
                .unwrap_or_else(|(i, piece)| panic!("{backend:?} rejects token {i} {piece:?}"));
            let rejected = replay(&model, ToolFormat::Qwen3, backend, &unquoted)
                .err()
                .unwrap_or_else(|| panic!("{backend:?} accepted an unquoted tool name"));
            assert!(
                rejected.1.trim_start().starts_with('w'),
                "{backend:?}: {rejected:?}"
            );
        }
    }

    #[test]
    fn end_of_turn_is_allowed_after_a_complete_call() {
        for (format, call) in COMPLETE_CALLS {
            let Some(model) = test_vocab::vocab_model(vocab_for(format)) else {
                continue;
            };
            for backend in BACKENDS {
                let mut sampler =
                    replay(&model, format, backend, call).unwrap_or_else(|(i, piece)| {
                        panic!("{format:?} {backend:?} rejects token {i} {piece:?}")
                    });
                assert!(
                    end_of_turn_allowed(&model, &mut sampler),
                    "{format:?} {backend:?}: no end-of-turn token after a complete call"
                );
                // Not in the middle of a call.
                let trigger = format.grammar(&[write_file_tool()]).unwrap().triggers[0]
                    .value
                    .clone();
                let mut open = replay(&model, format, backend, &trigger).unwrap();
                assert!(
                    !end_of_turn_allowed(&model, &mut open),
                    "{format:?} {backend:?}: end of turn allowed right after {trigger:?}"
                );
            }
        }
    }

    #[test]
    fn special_token_is_not_value_text() {
        // (format, prefix inside a string value, a control token that is not
        // the value's terminator). llguidance treats a token as special when
        // its piece is empty without the `special` flag, which holds for turn
        // and end-of-sequence tokens; Gemma's tool-call delimiters render as
        // plain text and are ordinary value text for it.
        let cases = [
            (ToolFormat::QwenFunction, CONTENT_PREFIX, "<|im_start|>"),
            (
                ToolFormat::Gemma4,
                "<|tool_call>call:write_file{path:<|\"|>a.py",
                "<turn|>",
            ),
            (
                ToolFormat::FunctionGemma,
                "<start_function_call>call:write_file{path:<escape>a.py",
                "<eos>",
            ),
        ];
        for (format, prefix, special) in cases {
            let Some(model) = test_vocab::vocab_model(vocab_for(format)) else {
                continue;
            };
            let grammar = format.grammar(&[write_file_tool()]).unwrap();
            let (special, text) = (token(&model, special), token(&model, "x"));
            let mut sampler = sampler_after(&model, grammar, prefix, GrammarBackend::Llguidance);
            assert!(sampler.allows(text), "{format:?}: ordinary text");
            assert!(
                !sampler.allows(special),
                "{format:?}: special token in a value"
            );
        }
    }

    fn tool_with_options_schema(options: serde_json::Value) -> Tool {
        let mut tool = write_file_tool();
        tool.function.parameters["properties"]["options"] = options;
        tool
    }

    #[test]
    fn uncompilable_schema_falls_back_to_generic_grammar() {
        use crate::tools::llg::Tier;
        let Some(model) = vocab_model() else { return };
        let build = |options: serde_json::Value| {
            let tool = tool_with_options_schema(options);
            let grammar = ToolFormat::QwenFunction
                .grammar(std::slice::from_ref(&tool))
                .unwrap();
            let mut result = template_result(grammar);
            result.tools = vec![tool];
            let cfg: LlamaCppConfig =
                serde_json::from_value(serde_json::json!({ "model": "vocab-only" })).unwrap();
            build_tool_sampler(
                &model,
                &result,
                &SamplingParams::from_config(&cfg, None),
                GrammarBackend::Llguidance,
            )
            .expect("request path succeeds")
        };

        // Control: a compilable object schema stays on the schema tier.
        let before = fallbacks();
        let sampler =
            build(serde_json::json!({"type": "object", "properties": {"a": {"type": "integer"}}}));
        assert_eq!(sampler.llg_tier(), Some(Tier::Schema));
        assert_eq!(fallbacks() - before, 0);

        // An unresolvable `$ref` cannot be compiled.
        let before = fallbacks();
        let sampler = build(serde_json::json!({"type": "object", "$ref": "#/definitions/missing"}));
        assert_eq!(sampler.llg_tier(), Some(Tier::Generic));
        assert_eq!(fallbacks() - before, 1);
    }

    fn fallbacks() -> u32 {
        crate::tools::llg::test_hooks::FALLBACKS.with(std::cell::Cell::get)
    }

    #[test]
    fn matcher_error_switches_to_gbnf() {
        let Some(model) = vocab_model() else { return };
        let grammar = ToolFormat::QwenFunction
            .grammar(&[write_file_tool()])
            .unwrap();
        let before = fallbacks();
        let mut sampler = sampler_after(&model, grammar, "<tool_call>", GrammarBackend::Llguidance);
        assert!(sampler.uses_llguidance());
        sampler.inject_llg_error();
        // `s` is forbidden right after the trigger. Only a GBNF sampler that
        // replayed `<tool_call>` knows that.
        let bad = model.vocab().tokenize(b"s", false, true)[0];
        assert!(!sampler.allows(bad), "constraint was dropped");
        assert!(!sampler.uses_llguidance());
        assert_eq!(fallbacks() - before, 1);
        // The switch happens once; further use logs nothing more.
        let newline = model.vocab().tokenize(b"\n", false, true)[0];
        assert!(sampler.allows(newline));
        sampler.accept(newline);
        assert_eq!(fallbacks() - before, 1);
    }

    #[test]
    fn llguidance_build_failure_uses_gbnf() {
        let Some(model) = vocab_model() else { return };
        let before = fallbacks();
        crate::tools::llg::test_hooks::FORCE_BUILD_ERROR.with(|f| f.set(true));
        let mut sampler = sampler_for_config(&model, serde_json::json!({}));
        crate::tools::llg::test_hooks::FORCE_BUILD_ERROR.with(|f| f.set(false));
        assert!(!sampler.uses_llguidance());
        assert_eq!(fallbacks() - before, 1);
        // The request path works: the GBNF grammar constrains it.
        for token in model.vocab().tokenize(b"<tool_call>", false, true) {
            sampler.accept(token);
        }
        let bad = model.vocab().tokenize(b"s", false, true)[0];
        assert!(!sampler.allows(bad));
    }

    /// Logits where `favourite` beats `runner_up`, which beats everything else.
    fn two_peak_logits(
        model: &LlamaModel,
        favourite: LlamaToken,
        runner_up: LlamaToken,
    ) -> Vec<f32> {
        let mut logits = vec![0.0f32; model.n_vocab() as usize];
        logits[favourite.0 as usize] = 10.0;
        logits[runner_up.0 as usize] = 5.0;
        logits
    }

    #[test]
    fn allowed_favourite_is_kept_without_full_mask() {
        let Some(model) = vocab_model() else { return };
        let grammar = ToolFormat::QwenFunction
            .grammar(&[write_file_tool()])
            .unwrap();
        let tokens = |text: &str| model.vocab().tokenize(text.as_bytes(), false, true);
        let favourite = tokens("s")[0];
        let runner_up = tokens("t")[0];
        let logits = two_peak_logits(&model, favourite, runner_up);
        for backend in [GrammarBackend::Gbnf, GrammarBackend::Llguidance] {
            // Inside a value every text token is allowed.
            let mut sampler = sampler_after(&model, grammar.clone(), CONTENT_PREFIX, backend);
            assert_eq!(sampler.sample_logits(&logits), favourite, "{backend:?}");
            assert_eq!(sampler.full_masks(), 0, "{backend:?}: mask was computed");

            // Control: right after the trigger `s` is forbidden, so the
            // counter must see exactly one full mask.
            let mut sampler = sampler_after(&model, grammar.clone(), "<tool_call>", backend);
            assert_ne!(sampler.sample_logits(&logits), favourite, "{backend:?}");
            assert_eq!(sampler.full_masks(), 1, "{backend:?}");
        }
    }

    #[test]
    fn forced_end_of_reasoning_keeps_grammar_state() {
        let Some(model) = vocab_model() else { return };
        let grammar = ToolFormat::QwenFunction
            .grammar(&[write_file_tool()])
            .unwrap();
        let tokens = |text: &str| model.vocab().tokenize(text.as_bytes(), false, true);
        let bad = tokens("s")[0];
        let newline = tokens("\n")[0];
        for backend in [GrammarBackend::Gbnf, GrammarBackend::Llguidance] {
            let result = template_result(grammar.clone());
            let cfg: LlamaCppConfig =
                serde_json::from_value(serde_json::json!({ "model": "vocab-only" })).unwrap();
            let params = SamplingParams::from_config(&cfg, None);
            let mut sampler =
                build_tool_sampler(&model, &result, &params, backend).expect("sampler");
            // `force_text` hands every forced token to `accept_token`.
            for token in tokens("</think>\n") {
                sampler.accept_token(token);
            }
            assert!(sampler.allows(bad), "{backend:?}: grammar active too early");
            for token in tokens("<tool_call>") {
                sampler.accept_token(token);
            }
            assert!(!sampler.allows(bad), "{backend:?}: grammar not active");
            assert!(sampler.allows(newline), "{backend:?}");
        }
    }

    /// Speculative (MTP) decoding on a real MTP model, with tools. Needs a
    /// 521 MB model, so it only runs with `QMT_RUN_LIVE_LLAMA_MTP=1` (plus
    /// `QMT_LLAMA_VOCAB_DOWNLOAD=1` unless the model is already cached).
    #[test]
    #[ignore = "live: set QMT_RUN_LIVE_LLAMA_MTP=1 and use --run-ignored"]
    fn speculative_decoding_respects_the_tool_grammar() {
        use crate::provider::LlamaCppProvider;
        use crate::test_vocab::{MTP_MODEL, VocabError, ensure_full_model};
        use querymt::chat::ChatMessage;

        if std::env::var_os("QMT_RUN_LIVE_LLAMA_MTP").is_none() {
            eprintln!("SKIPPED: set QMT_RUN_LIVE_LLAMA_MTP=1 to run the live MTP check");
            return;
        }
        let path = match ensure_full_model(&MTP_MODEL) {
            Ok(path) => path,
            Err(VocabError::Unavailable(why)) => {
                eprintln!("SKIPPED: MTP model unavailable: {why}");
                return;
            }
            Err(changed) => panic!("{changed}"),
        };
        let cfg: LlamaCppConfig = serde_json::from_value(serde_json::json!({
            "model": path,
            "max_tokens": 160,
            "temperature": 0.0,
            "enable_thinking": false,
            "speculative": { "type": "mtp" },
        }))
        .unwrap();
        let provider = LlamaCppProvider::new(cfg).expect("load MTP model");
        let model = &provider.model;
        let layers = model
            .meta_val_str("general.architecture")
            .and_then(|arch| model.meta_val_str(&format!("{arch}.nextn_predict_layers")))
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0);
        assert!(layers > 0, "pinned model has no bundled MTP layers");

        let tools = [write_file_tool()];
        let messages = [ChatMessage::user()
            .text("Create the file hello.py containing a Python hello world program.")
            .build()];
        for backend in [GrammarBackend::Gbnf, GrammarBackend::Llguidance] {
            let mut cfg = provider.cfg.clone();
            cfg.grammar_backend = Some(backend);
            let result =
                crate::template::apply_template_with_tools(model, &cfg, &messages, &tools, None)
                    .expect("template");
            assert!(result.grammar.is_some(), "{backend:?}: no tool grammar");
            let generated = crate::tools::generate_with_tools(
                model,
                &cfg,
                provider.mtp_model.as_ref(),
                &result,
                160,
                None,
                None,
                &[],
            )
            .expect("generate");

            // Replay what was delivered through a fresh grammar: after the
            // trigger every token must have been allowed.
            let params = SamplingParams::from_config(&cfg, None);
            let mut sampler =
                build_tool_sampler(model, &result, &params, backend).expect("sampler");
            assert_eq!(
                sampler.uses_llguidance(),
                backend == GrammarBackend::Llguidance
            );
            for token in model
                .vocab()
                .tokenize(generated.text.as_bytes(), false, true)
            {
                assert!(
                    sampler.allows(token),
                    "{backend:?}: delivered token {token:?} violates the grammar in {:?}",
                    generated.text
                );
                sampler.accept(token);
            }

            let (_, _, calls, _) =
                crate::tools::parse_tool_response(&result, &generated.text).expect("parse");
            let calls = calls
                .unwrap_or_else(|| panic!("{backend:?}: no tool call in {:?}", generated.text));
            assert_eq!(calls[0].function.name, "write_file", "{backend:?}");
            let args: serde_json::Value =
                serde_json::from_str(&calls[0].function.arguments).expect("arguments are JSON");
            for key in ["path", "content"] {
                assert!(args[key].is_string(), "{backend:?}: {key} in {args}");
            }
        }
    }

    /// Per-token grammar cost inside a long string value. Run with
    /// `QMT_RUN_GRAMMAR_BENCH=1 cargo nextest run --release -p qmt-llama-cpp
    /// --run-ignored only --no-capture -E 'test(/llguidance_value_tokens/)'`.
    #[test]
    #[ignore = "benchmark: set QMT_RUN_GRAMMAR_BENCH=1 and use --run-ignored"]
    fn llguidance_value_tokens_stay_under_one_millisecond() {
        if std::env::var_os("QMT_RUN_GRAMMAR_BENCH").is_none() {
            eprintln!("SKIPPED: set QMT_RUN_GRAMMAR_BENCH=1 to run the grammar benchmark");
            return;
        }
        let Some(model) = vocab_model() else { return };
        let base = ToolFormat::QwenFunction
            .grammar(&[write_file_tool()])
            .unwrap();
        let with_value_rules = |rules: String| {
            let mut g = base.clone();
            g.grammar = g
                .grammar
                .lines()
                .filter(|l| !l.starts_with("value-text"))
                .map(str::to_string)
                .chain(std::iter::once(rules))
                .collect::<Vec<_>>()
                .join("\n");
            g
        };
        let dfa = crate::gbnf::ExclusionDfa::new("</parameter>");
        let variants = [
            (
                "gbnf [^<]* (pre-fix, lossy)",
                with_value_rules("value-text ::= [^<]*".into()),
                GrammarBackend::Gbnf,
            ),
            (
                "gbnf exclusion, uncollapsed",
                with_value_rules(dfa.rules_uncollapsed("value-text")),
                GrammarBackend::Gbnf,
            ),
            (
                "gbnf exclusion, collapsed",
                base.clone(),
                GrammarBackend::Gbnf,
            ),
            ("llguidance", base.clone(), GrammarBackend::Llguidance),
        ];
        // No `<`: the pre-fix variant would reject it, and every variant
        // must accept the same body for the comparison to be fair.
        let body = "def tour_length(tour, d):\n    total = 0.0\n    for i in range(len(tour)):\n        a = tour[i]\n        b = tour[(i + 1) % len(tour)]\n        total += d[a][b]\n    return total\n\n\ndef two_opt(tour, d):\n    improved = True\n    while improved:\n        improved = False\n        for i in range(1, len(tour) - 2):\n            for j in range(i + 1, len(tour)):\n                if j - i == 1:\n                    continue\n                delta = d[tour[i - 1]][tour[j - 1]] + d[tour[i]][tour[j % len(tour)]]\n                if delta >= 0:\n                    continue\n                tour[i:j] = reversed(tour[i:j])\n                improved = True\n    return tour\n";
        let tokens = model.vocab().tokenize(body.as_bytes(), false, true);
        let prefix = "<tool_call>\n<function=write_file>\n<parameter=path>\na.py\n</parameter>\n<parameter=content>\n";
        // The first llguidance build on a model walks the whole vocabulary
        // to make the token trie; later builds reuse it (design D6).
        let started = std::time::Instant::now();
        drop(sampler_after(
            &model,
            base.clone(),
            prefix,
            GrammarBackend::Llguidance,
        ));
        let first = started.elapsed();
        let started = std::time::Instant::now();
        drop(sampler_after(
            &model,
            base.clone(),
            prefix,
            GrammarBackend::Llguidance,
        ));
        eprintln!(
            "llguidance first build (token trie) {:.1} ms, cached build {:.2} ms",
            first.as_secs_f64() * 1e3,
            started.elapsed().as_secs_f64() * 1e3
        );
        eprintln!(
            "{} body tokens, vocabulary {}",
            tokens.len(),
            model.n_vocab()
        );
        let mut llg_mean = None;
        for (name, grammar, backend) in variants {
            let mut sampler = sampler_after(&model, grammar, prefix, backend);
            let (mut full, mut check) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
            for &token in &tokens {
                let mut cands = full_vocab(&model);
                let t = std::time::Instant::now();
                sampler.mask(&mut cands);
                full += t.elapsed();
                let t = std::time::Instant::now();
                let ok = sampler.allows(token);
                check += t.elapsed();
                assert!(ok, "{name}: grammar rejected body token {token}");
                sampler.accept(token);
            }
            let n = tokens.len() as u32;
            if backend == GrammarBackend::Llguidance {
                assert!(sampler.uses_llguidance());
                llg_mean = Some((full + check) / n);
            }
            eprintln!(
                "{name:32} full-vocab mask {:>9.3} ms/token   single-token check {:>8.1} us/token",
                (full / n).as_secs_f64() * 1e3,
                (check / n).as_secs_f64() * 1e6
            );
        }
        // Mask plus check per token: more than a request pays, since the
        // mask is only computed for rejected tokens.
        let mean = llg_mean.expect("llguidance variant ran");
        assert!(
            mean < std::time::Duration::from_millis(1),
            "llguidance mean {mean:?} per token is not under 1 ms"
        );
    }
}
