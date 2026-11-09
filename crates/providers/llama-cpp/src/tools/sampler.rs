use crate::common_chat::ChatTemplateResult;
use crate::config::LlamaCppConfig;
use llama_cpp_2::model::LlamaModel;
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
                let tokens = model.vocab().tokenize(trigger.value.as_bytes(), false, true);
                match tokens.as_slice() {
                    [token] => {
                        log::debug!(
                            "build_tool_sampler: trigger '{}' tokenized to single token {}",
                            trigger.value,
                            token
                        );
                        trigger_tokens.push(*token);
                    }
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
            build_standard_sampler(model, params),
        ]));
    }

    #[cfg(feature = "common")]
    log::warn!(
        "build_tool_sampler: no tool grammar present in ChatTemplateResult — sampling unconstrained"
    );

    #[cfg(not(feature = "common"))]
    let _ = (model, result);

    Ok(build_standard_sampler(model, params))
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
) -> Result<LlamaSampler, LLMError> {
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
    Ok(LlamaSampler::chain_simple([
        LlamaSampler::from(matcher),
        build_standard_sampler(model, params),
    ]))
}

#[cfg(not(feature = "llguidance"))]
pub(crate) fn build_structured_sampler(
    model: &Arc<LlamaModel>,
    schema: &serde_json::Value,
    params: &SamplingParams,
) -> Result<LlamaSampler, LLMError> {
    let grammar = llama_cpp_2::json_schema_to_grammar(&schema.to_string())
        .map_err(|e| LLMError::ProviderError(format!("Failed to compile JSON schema: {e}")))?;
    let constraint = LlamaSampler::grammar(model, &grammar, "root")
        .map_err(|e| LLMError::ProviderError(format!("Failed to build JSON grammar: {e}")))?;
    Ok(LlamaSampler::chain_simple([
        constraint,
        build_standard_sampler(model, params),
    ]))
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
        let result = template_result(grammar);
        let cfg: LlamaCppConfig =
            serde_json::from_value(serde_json::json!({ "model": "vocab-only" })).unwrap();
        let params = SamplingParams::from_config(&cfg, None);
        let mut sampler = build_tool_sampler(model, &result, &params).expect("sampler");
        for token in model.vocab().tokenize(prefix.as_bytes(), false, true) {
            sampler.accept(token);
        }
        let mut candidates = LlamaTokenDataArray::from_iter(
            (0..model.n_vocab()).map(|id| LlamaTokenData::new(LlamaToken(id), 0.0, 0.0)),
            false,
        );
        sampler.apply(&mut candidates);
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
}
