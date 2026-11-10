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
}

enum Constraint {
    /// A llama.cpp sampler that masks logits (GBNF, or llguidance for
    /// structured output).
    Sampler(LlamaSampler),
    /// Lazy llguidance tool grammar.
    #[cfg(feature = "common")]
    Llg(Box<crate::tools::llg::LlgToolGrammar>),
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
            Self::Llg(grammar) => grammar.allows(token),
        }
    }

    fn apply(&mut self, cur: &mut LlamaTokenDataArray) {
        match self {
            Self::Sampler(grammar) => grammar.apply(cur),
            #[cfg(feature = "common")]
            Self::Llg(grammar) => grammar.apply(cur),
        }
    }

    fn accept(&mut self, token: LlamaToken) {
        match self {
            Self::Sampler(grammar) => grammar.accept(token),
            #[cfg(feature = "common")]
            Self::Llg(grammar) => grammar.accept(token),
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
        let Self {
            constraint,
            chain,
            cur,
            probe,
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
                log::debug!("build_tool_sampler: using llguidance tool grammar");
                return Ok(ConstrainedSampler::new(
                    Some(Constraint::Llg(Box::new(grammar))),
                    chain,
                ));
            }
            Ok(None) => log::debug!(
                "build_tool_sampler: no llguidance port for {:?}; using GBNF",
                tool_grammar.format
            ),
            Err(error) => {
                log::warn!("build_tool_sampler: llguidance grammar failed ({error}); using GBNF")
            }
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
    use llama_cpp_2::model::params::LlamaModelParams;
    use llama_cpp_2::token::LlamaToken;
    use llama_cpp_2::token::data::LlamaTokenData;
    use llama_cpp_2::token::data_array::LlamaTokenDataArray;
    use querymt::chat::{FunctionTool, Tool};
    use std::path::PathBuf;

    /// Pinned to the llama.cpp commit that added the vocab file.
    const VOCAB_URL: &str = "https://raw.githubusercontent.com/ggml-org/llama.cpp/42532afff43910e619a650c1704525b3acbbec5a/models/ggml-vocab-qwen35.gguf";

    /// `QMT_LLAMA_VOCAB_GGUF` overrides the location; otherwise the file is
    /// downloaded once into the temp dir.
    fn vocab_path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("QMT_LLAMA_VOCAB_GGUF") {
            return Some(PathBuf::from(path));
        }
        let dir = std::env::temp_dir().join("qmt-llama-vocab");
        let path = dir.join("ggml-vocab-qwen35.gguf");
        if path.metadata().is_ok_and(|m| m.len() > 1_000_000) {
            return Some(path);
        }
        std::fs::create_dir_all(&dir).ok()?;
        // Unique per process and thread so concurrent test runs never share it.
        let thread: String = format!("{:?}", std::thread::current().id())
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect();
        let tmp = dir.join(format!(
            "ggml-vocab-qwen35.gguf.{}.{thread}.part",
            std::process::id()
        ));
        let ok = std::process::Command::new("curl")
            .args(["-fsSL", "--retry", "3", "-o"])
            .arg(&tmp)
            .arg(VOCAB_URL)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            let _ = std::fs::remove_file(&tmp);
            return None;
        }
        if std::fs::rename(&tmp, &path).is_ok() {
            return Some(path);
        }
        // Another runner may have finished first; accept its file.
        let _ = std::fs::remove_file(&tmp);
        path.metadata()
            .is_ok_and(|m| m.len() > 1_000_000)
            .then_some(path)
    }

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
    fn grammar_does_not_mask_lt_inside_string_values() {
        let Some(path) = vocab_path() else {
            eprintln!("SKIPPED: vocab-only GGUF unavailable (offline?); set QMT_LLAMA_VOCAB_GGUF");
            return;
        };
        let backend = crate::backend::llama_backend().expect("backend");
        let model = Arc::new(
            LlamaModel::load_from_file(
                &backend,
                &path,
                &LlamaModelParams::default().with_vocab_only(true),
            )
            .expect("load vocab-only gguf"),
        );

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
        let Some(path) = vocab_path() else {
            eprintln!("SKIPPED: vocab-only GGUF unavailable (offline?); set QMT_LLAMA_VOCAB_GGUF");
            return;
        };
        let backend = crate::backend::llama_backend().expect("backend");
        let model = Arc::new(
            LlamaModel::load_from_file(
                &backend,
                &path,
                &LlamaModelParams::default().with_vocab_only(true),
            )
            .expect("load vocab-only gguf"),
        );
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
        let Some(path) = vocab_path() else {
            eprintln!("SKIPPED: vocab-only GGUF unavailable (offline?); set QMT_LLAMA_VOCAB_GGUF");
            return None;
        };
        let backend = crate::backend::llama_backend().expect("backend");
        Some(Arc::new(
            LlamaModel::load_from_file(
                &backend,
                &path,
                &LlamaModelParams::default().with_vocab_only(true),
            )
            .expect("load vocab-only gguf"),
        ))
    }

    const CONTENT_PREFIX: &str = "<tool_call>\n<function=write_file>\n<parameter=path>\na.py\n</parameter>\n<parameter=content>\nif row[j]";

    /// The llguidance port must keep the `<` fix and still close parameters.
    #[test]
    fn llguidance_backend_matches_gbnf_on_lt_and_close() {
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
    fn rejection_sampling_matches_grammar_first() {
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

    /// Per-token grammar cost inside a long string value. Run with
    /// `cargo test --release -p qmt-llama-cpp grammar_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn grammar_cost_benchmark() {
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
        for (name, grammar, backend) in variants {
            let mut sampler = sampler_after(
                &model,
                grammar,
                "<tool_call>\n<function=write_file>\n<parameter=path>\na.py\n</parameter>\n<parameter=content>\n",
                backend,
            );
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
            eprintln!(
                "{name:32} full-vocab mask {:>9.3} ms/token   single-token check {:>8.1} us/token",
                (full / n).as_secs_f64() * 1e3,
                (check / n).as_secs_f64() * 1e6
            );
        }
    }
}
