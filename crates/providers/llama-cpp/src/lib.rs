use async_trait::async_trait;
use futures::Stream;
use futures::channel::mpsc;
use hf_hub::api::sync::ApiBuilder as SyncApiBuilder;
use hf_hub::api::tokio::ApiBuilder as AsyncApiBuilder;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{
    AddBos, ChatTemplateResult, GrammarTriggerType, LlamaChatMessage, LlamaChatTemplate, LlamaModel,
};
use llama_cpp_2::openai::OpenAIChatTemplateParams;
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::{LogOptions, send_logs_to_tracing};
use querymt::chat::{
    ChatMessage, ChatProvider, ChatResponse, ChatRole, FinishReason, MessageType, Tool,
};
use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
use querymt::embedding::EmbeddingProvider;
use querymt::error::LLMError;
use querymt::plugin::{Fut, LLMProviderFactory};
use querymt::{LLMProvider, Usage};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

const DEFAULT_MAX_TOKENS: u32 = 256;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct LlamaCppConfig {
    /// Path to a local GGUF model file.
    pub model_path: String,
    /// Optional display name for the model.
    pub model: Option<String>,
    /// Maximum tokens to generate.
    pub max_tokens: Option<u32>,
    /// Sampling temperature; set to 0 for greedy.
    pub temperature: Option<f32>,
    /// Top-p sampling.
    pub top_p: Option<f32>,
    /// Top-k sampling.
    pub top_k: Option<u32>,
    /// System prompt to prepend to chat requests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<String>,
    /// Override model context length.
    pub n_ctx: Option<u32>,
    /// Batch size for llama.cpp decoding.
    pub n_batch: Option<u32>,
    /// Threads for evaluation.
    pub n_threads: Option<i32>,
    /// Threads for batch evaluation.
    pub n_threads_batch: Option<i32>,
    /// GPU layers to offload, if supported.
    pub n_gpu_layers: Option<u32>,
    /// RNG seed for sampling.
    pub seed: Option<u32>,
    /// Explicit chat template name (defaults to model's template).
    pub chat_template: Option<String>,
    /// Disable llama.cpp chat template usage and fall back to a simple prompt format.
    pub use_chat_template: Option<bool>,
    /// Control whether to add BOS when tokenizing prompts.
    pub add_bos: Option<bool>,
    /// Logging destination for llama.cpp output.
    pub log: Option<LlamaCppLogMode>,
    /// Enable high-throughput HuggingFace Hub downloads. Uses multiple parallel
    /// connections to saturate high-bandwidth connections (>500MB/s). This will
    /// heavily utilize CPU cores during download. Only recommended for cloud
    /// instances with high CPU and bandwidth.
    pub fast_download: Option<bool>,
    /// Enable thinking/reasoning output from the model.
    /// When true, the template is rendered with thinking support and
    /// `<think>` blocks are parsed into separate reasoning_content.
    /// Defaults to true.
    pub enable_thinking: Option<bool>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LlamaCppLogMode {
    Stderr,
    Tracing,
    Off,
}

struct LlamaCppProvider {
    model: Arc<LlamaModel>,
    cfg: LlamaCppConfig,
}

#[derive(Debug)]
struct LlamaCppChatResponse {
    text: String,
    thinking: Option<String>,
    tool_calls: Option<Vec<querymt::ToolCall>>,
    finish_reason: FinishReason,
    usage: Usage,
}

impl fmt::Display for LlamaCppChatResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.text)
    }
}

impl ChatResponse for LlamaCppChatResponse {
    fn text(&self) -> Option<String> {
        Some(self.text.clone())
    }

    fn thinking(&self) -> Option<String> {
        self.thinking.clone()
    }

    fn tool_calls(&self) -> Option<Vec<querymt::ToolCall>> {
        self.tool_calls.clone()
    }

    fn usage(&self) -> Option<Usage> {
        Some(self.usage.clone())
    }

    fn finish_reason(&self) -> Option<FinishReason> {
        Some(self.finish_reason)
    }
}

struct GeneratedText {
    text: String,
    usage: Usage,
}

fn llama_backend() -> Result<std::sync::MutexGuard<'static, LlamaBackend>, LLMError> {
    static BACKEND: OnceLock<Result<Mutex<LlamaBackend>, String>> = OnceLock::new();
    let backend = BACKEND
        .get_or_init(|| {
            LlamaBackend::init()
                .map(Mutex::new)
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| LLMError::ProviderError(e.clone()))?;
    backend
        .lock()
        .map_err(|_| LLMError::ProviderError("Llama backend lock poisoned".to_string()))
}

impl LlamaCppProvider {
    fn resolve_model_path(raw: &str, fast: bool) -> Result<PathBuf, LLMError> {
        let Some(rest) = raw.strip_prefix("hf:") else {
            return Ok(PathBuf::from(raw));
        };
        let mut parts = rest.splitn(2, ':');
        let repo = parts.next().unwrap_or("").trim();
        let file = parts.next().unwrap_or("").trim();
        if repo.is_empty() || file.is_empty() {
            return Err(LLMError::InvalidRequest(
                "hf: model_path must be formatted as hf:<repo>:<file>".into(),
            ));
        }

        // High-throughput async download when explicitly enabled and a tokio runtime is available
        if fast {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let repo = repo.to_string();
                let file = file.to_string();
                let path = tokio::task::block_in_place(|| {
                    handle.block_on(async {
                        let api = AsyncApiBuilder::new()
                            .with_progress(true)
                            .high()
                            .build()
                            .map_err(|e| LLMError::ProviderError(e.to_string()))?;
                        api.model(repo)
                            .get(&file)
                            .await
                            .map_err(|e| LLMError::ProviderError(e.to_string()))
                    })
                })?;
                return Ok(path);
            }
        }

        // Standard sync download (default, or fallback when no tokio runtime available)
        let api = SyncApiBuilder::new()
            .with_progress(true)
            .build()
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        let path = api
            .model(repo.to_string())
            .get(file)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        Ok(path)
    }

    fn build_fallback_sampler(&self) -> LlamaSampler {
        let seed = self.cfg.seed.unwrap_or(1234);
        LlamaSampler::chain_simple([
            LlamaSampler::temp(0.7),
            LlamaSampler::top_p(0.9, 1),
            LlamaSampler::dist(seed),
        ])
    }

    fn new(cfg: LlamaCppConfig) -> Result<Self, LLMError> {
        let mut backend = llama_backend()?;
        let log_mode = cfg.log.unwrap_or(LlamaCppLogMode::Off);
        match log_mode {
            LlamaCppLogMode::Stderr => {}
            LlamaCppLogMode::Tracing => send_logs_to_tracing(LogOptions::default()),
            LlamaCppLogMode::Off => backend.void_logs(),
        }
        let model_path =
            Self::resolve_model_path(&cfg.model_path, cfg.fast_download.unwrap_or(false))?;
        let model_path = Path::new(&model_path);
        if !model_path.exists() {
            return Err(LLMError::InvalidRequest(format!(
                "Model path does not exist: {}",
                model_path.display()
            )));
        }

        let mut params = LlamaModelParams::default();
        if let Some(n_gpu_layers) = cfg.n_gpu_layers {
            params = params.with_n_gpu_layers(n_gpu_layers);
        }

        let model = LlamaModel::load_from_file(&*backend, model_path, &params)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;

        Ok(Self {
            model: Arc::new(model),
            cfg,
        })
    }

    /// Utility function to escape regex special characters.
    fn regex_escape(value: &str) -> String {
        let mut escaped = String::with_capacity(value.len());
        for ch in value.chars() {
            match ch {
                '.' | '^' | '$' | '|' | '(' | ')' | '*' | '+' | '?' | '[' | ']' | '{' | '}'
                | '\\' => {
                    escaped.push('\\');
                    escaped.push(ch);
                }
                _ => escaped.push(ch),
            }
        }
        escaped
    }

    /// Utility function to anchor a regex pattern.
    fn anchor_pattern(pattern: &str) -> String {
        if pattern.is_empty() {
            return "^$".to_string();
        }
        let mut anchored = String::new();
        if !pattern.starts_with('^') {
            anchored.push('^');
        }
        anchored.push_str(pattern);
        if !pattern.ends_with('$') {
            anchored.push('$');
        }
        anchored
    }

    /// Convert querymt Tool objects to OpenAI-compatible JSON string.
    fn convert_tools_to_json(&self, tools: &[Tool]) -> Result<String, LLMError> {
        serde_json::to_string(tools).map_err(|e| LLMError::ProviderError(e.to_string()))
    }

    /// Build OpenAI-compatible JSON messages from ChatMessage array for tool-aware conversations.
    fn build_messages_json_for_tools(&self, messages: &[ChatMessage]) -> Result<String, LLMError> {
        let mut json_messages = Vec::new();

        // Add system message if configured
        if !self.cfg.system.is_empty() {
            let system = self.cfg.system.join("\n\n");
            json_messages.push(serde_json::json!({
                "role": "system",
                "content": system
            }));
        }

        for msg in messages {
            match &msg.message_type {
                MessageType::Text => {
                    let role = match msg.role {
                        ChatRole::User => "user",
                        ChatRole::Assistant => "assistant",
                    };

                    // For assistant messages, separate <think> blocks from content
                    // into reasoning_content for the template engine.
                    // If thinking was already extracted (msg.thinking is Some), use it.
                    // Otherwise, extract from content as a fallback for messages
                    // stored before thinking extraction was available.
                    let (thinking, content) = if msg.thinking.is_some() {
                        (msg.thinking.clone(), msg.content.clone())
                    } else if matches!(msg.role, ChatRole::Assistant) {
                        let (t, c) = querymt::chat::extract_thinking(&msg.content);
                        (t, c)
                    } else {
                        (None, msg.content.clone())
                    };

                    let mut json_msg = serde_json::json!({
                        "role": role,
                        "content": content
                    });
                    if let Some(ref t) = thinking {
                        if !t.is_empty() {
                            json_msg["reasoning_content"] = serde_json::json!(t);
                        }
                    }
                    json_messages.push(json_msg);
                }
                MessageType::ToolUse(tool_calls) => {
                    // Assistant message with tool calls in OpenAI format
                    let tool_calls_array: Vec<Value> = tool_calls
                        .iter()
                        .map(|tc| {
                            serde_json::json!({
                                "id": tc.id,
                                "type": tc.call_type,
                                "function": {
                                    "name": tc.function.name,
                                    "arguments": tc.function.arguments
                                }
                            })
                        })
                        .collect();

                    // Separate <think> blocks from content (fallback extraction)
                    let (thinking, clean_content) = if msg.thinking.is_some() {
                        (msg.thinking.clone(), msg.content.clone())
                    } else {
                        let (t, c) = querymt::chat::extract_thinking(&msg.content);
                        (t, c)
                    };

                    let content = if clean_content.is_empty() {
                        Value::Null
                    } else {
                        Value::String(clean_content)
                    };

                    let mut json_msg = serde_json::json!({
                        "role": "assistant",
                        "content": content,
                        "tool_calls": tool_calls_array
                    });
                    if let Some(ref t) = thinking {
                        if !t.is_empty() {
                            json_msg["reasoning_content"] = serde_json::json!(t);
                        }
                    }
                    json_messages.push(json_msg);
                }
                MessageType::ToolResult(results) => {
                    // Tool results - each result is a separate message with tool role
                    // Note: function.arguments contains the result content,
                    // function.name contains the tool name, and id is the tool_call_id
                    for result in results {
                        json_messages.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": result.id,
                            "name": result.function.name,
                            "content": result.function.arguments
                        }));
                    }
                }
                _ => {
                    return Err(LLMError::InvalidRequest(
                        "Only text and tool-related messages are supported by llama.cpp provider"
                            .into(),
                    ));
                }
            }
        }

        serde_json::to_string(&json_messages).map_err(|e| {
            LLMError::ProviderError(format!("Failed to serialize messages JSON: {}", e))
        })
    }

    /// Apply chat template with tools to generate prompt and grammar.
    fn apply_template_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[Tool],
    ) -> Result<ChatTemplateResult, LLMError> {
        let tools_json = self.convert_tools_to_json(tools)?;
        let messages_json = self.build_messages_json_for_tools(messages)?;

        log::debug!(
            "Applying chat template with {} messages and {} tools",
            messages.len(),
            tools.len()
        );
        log::debug!("Messages JSON: {}", messages_json);
        log::debug!("Tools JSON: {}", tools_json);

        let template = self
            .model
            .chat_template(self.cfg.chat_template.as_deref())
            .or_else(|_| LlamaChatTemplate::new("chatml"))
            .map_err(|e| LLMError::ProviderError(format!("Failed to get chat template: {}", e)))?;

        let params = OpenAIChatTemplateParams {
            messages_json: &messages_json,
            tools_json: Some(&tools_json),
            tool_choice: None,
            json_schema: None,
            grammar: None,
            reasoning_format: None,
            chat_template_kwargs: None,
            add_generation_prompt: true,
            use_jinja: true,
            parallel_tool_calls: false,
            enable_thinking: self.cfg.enable_thinking.unwrap_or(true),
            // BOS is handled by the tokenizer in generate_with_tools(),
            // not by the template engine, to avoid double-BOS.
            // See self.cfg.add_bos.
            add_bos: false,
            add_eos: false,
            parse_tool_calls: true,
        };

        let result = self
            .model
            .apply_chat_template_oaicompat(&template, &params)
            .map_err(|e| {
                LLMError::ProviderError(format!("Failed to apply chat template: {}", e))
            })?;

        log::debug!(
            "Template applied: prompt_len={}, has_grammar={}, triggers={}, stops={}, parse_tool_calls={}",
            result.prompt.len(),
            result.grammar.is_some(),
            result.grammar_triggers.len(),
            result.additional_stops.len(),
            result.parse_tool_calls
        );

        Ok(result)
    }

    /// Build a grammar-constrained sampler from a ChatTemplateResult.
    ///
    /// When a grammar is present, we use only `[grammar, greedy]` to match
    /// the reference llama.cpp examples. Mixing temperature / top-p / top-k
    /// with grammar sampling can corrupt the grammar state and trigger
    /// assertion failures in llama-grammar.cpp.
    fn build_tool_sampler(
        &self,
        result: &ChatTemplateResult,
        temperature: Option<f32>,
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
                            match self.model.str_to_token(&trigger.value, AddBos::Never) {
                                Ok(tokens) if tokens.len() == 1 => {
                                    trigger_tokens.push(tokens[0]);
                                }
                                _ => {
                                    trigger_patterns.push(Self::regex_escape(&trigger.value));
                                }
                            }
                        }
                        GrammarTriggerType::Pattern => {
                            trigger_patterns.push(trigger.value.clone());
                        }
                        GrammarTriggerType::PatternFull => {
                            trigger_patterns.push(Self::anchor_pattern(&trigger.value));
                        }
                    }
                }

                LlamaSampler::grammar_lazy_patterns(
                    &self.model,
                    grammar,
                    "root",
                    &trigger_patterns,
                    &trigger_tokens,
                )
                .ok()
            } else {
                // Build strict grammar sampler
                LlamaSampler::grammar(&self.model, grammar, "root").ok()
            };

            if let Some(g) = grammar_sampler {
                // Grammar + greedy only — no temp/top_p/top_k
                return LlamaSampler::chain_simple([g, LlamaSampler::greedy()]);
            }
        }

        // No grammar or grammar creation failed — fall back to standard sampler
        self.build_sampler(temperature)
    }

    /// Generate text with grammar-constrained sampling for tool calls.
    fn generate_with_tools(
        &self,
        result: &ChatTemplateResult,
        max_tokens: u32,
        temperature: Option<f32>,
    ) -> Result<GeneratedText, LLMError> {
        let backend = llama_backend()?;
        let add_bos = self.cfg.add_bos.unwrap_or(true);
        let tokens = self
            .model
            .str_to_token(
                &result.prompt,
                if add_bos {
                    AddBos::Always
                } else {
                    AddBos::Never
                },
            )
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;

        if tokens.is_empty() {
            return Err(LLMError::InvalidRequest(
                "Prompt tokenization resulted in an empty sequence".into(),
            ));
        }

        log::debug!(
            "Generating with tools: input_tokens={}, max_tokens={}, add_bos={}",
            tokens.len(),
            max_tokens,
            add_bos
        );

        if max_tokens == 0 {
            return Ok(GeneratedText {
                text: String::new(),
                usage: Usage {
                    input_tokens: tokens.len() as u32,
                    output_tokens: 0,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning_tokens: 0,
                },
            });
        }

        // Auto-size context when not explicitly configured (tool prompts are large)
        let n_ctx_needed = tokens.len() as u32 + max_tokens;
        let n_ctx = if let Some(configured_n_ctx) = self.cfg.n_ctx {
            configured_n_ctx
        } else {
            // Only allocate what we actually need; cap at the model's training
            // context to avoid GPU out-of-memory when n_ctx is not configured.
            n_ctx_needed.min(self.model.n_ctx_train())
        };

        log::debug!(
            "Context sizing: needed={}, configured={:?}, model_train={}, using={}",
            n_ctx_needed,
            self.cfg.n_ctx,
            self.model.n_ctx_train(),
            n_ctx
        );

        let mut ctx_params = LlamaContextParams::default();
        let n_ctx = NonZeroU32::new(n_ctx)
            .ok_or_else(|| LLMError::InvalidRequest("n_ctx must be greater than zero".into()))?;
        ctx_params = ctx_params.with_n_ctx(Some(n_ctx));
        ctx_params = ctx_params.with_n_batch(n_ctx.get());
        if let Some(n_threads) = self.cfg.n_threads {
            ctx_params = ctx_params.with_n_threads(n_threads);
        }
        if let Some(n_threads_batch) = self.cfg.n_threads_batch {
            ctx_params = ctx_params.with_n_threads_batch(n_threads_batch);
        }

        let mut ctx = self
            .model
            .new_context(&*backend, ctx_params)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;

        let n_ctx_total = ctx.n_ctx() as i32;
        let n_len_total = tokens.len() as i32 + max_tokens as i32;
        if n_len_total > n_ctx_total {
            return Err(LLMError::InvalidRequest(format!(
                "Prompt + max_tokens ({n_len_total}) exceeds context window ({n_ctx_total})"
            )));
        }

        let mut batch = LlamaBatch::new(n_ctx_total as usize, 1);

        let last_index = tokens.len().saturating_sub(1) as i32;
        for (i, token) in (0_i32..).zip(tokens.iter().copied()) {
            let is_last = i == last_index;
            batch
                .add(token, i, &[0], is_last)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        }

        ctx.decode(&mut batch)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;

        // Build preserved token set for special handling
        let mut preserved = HashSet::new();
        for token_str in &result.preserved_tokens {
            if let Ok(preserved_tokens) = self.model.str_to_token(token_str, AddBos::Never) {
                if preserved_tokens.len() == 1 {
                    preserved.insert(preserved_tokens[0]);
                }
            }
        }

        let mut sampler = self.build_tool_sampler(result, temperature);
        let mut n_cur = batch.n_tokens();
        let mut output_tokens = 0u32;
        let mut output = String::new();
        let mut decoder = encoding_rs::UTF_8.new_decoder();

        while n_cur < n_len_total {
            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            if self.model.is_eog_token(token) {
                break;
            }

            let special = preserved.contains(&token);
            let bytes = self
                .model
                .token_to_piece_bytes(token, 128, special, None)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
            let chunk = match self
                .model
                .token_to_piece(token, &mut decoder, special, None)
            {
                Ok(piece) => piece,
                Err(_) => String::from_utf8_lossy(&bytes).to_string(),
            };
            output.push_str(&chunk);

            // Check additional stop sequences
            if result
                .additional_stops
                .iter()
                .any(|stop| !stop.is_empty() && output.ends_with(stop))
            {
                break;
            }

            batch.clear();
            batch
                .add(token, n_cur, &[0], true)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
            n_cur += 1;
            output_tokens += 1;

            ctx.decode(&mut batch)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        }

        // Trim matched stop sequences
        for stop in &result.additional_stops {
            if !stop.is_empty() && output.ends_with(stop) {
                let new_len = output.len().saturating_sub(stop.len());
                output.truncate(new_len);
                break;
            }
        }

        Ok(GeneratedText {
            text: output,
            usage: Usage {
                input_tokens: tokens.len() as u32,
                output_tokens,
                cache_read: 0,
                cache_write: 0,
                reasoning_tokens: 0,
            },
        })
    }

    /// Parse the generated response using the ChatTemplateResult to extract tool calls.
    ///
    /// Returns (content, thinking, tool_calls, finish_reason).
    fn parse_tool_response(
        result: &ChatTemplateResult,
        text: &str,
    ) -> Result<
        (
            String,
            Option<String>,
            Option<Vec<querymt::ToolCall>>,
            FinishReason,
        ),
        LLMError,
    > {
        log::debug!("Parsing tool response: text_len={}", text.len());
        log::debug!("Raw generated text: {}", text);

        let parsed_json = result.parse_response_oaicompat(text, false).map_err(|e| {
            log::debug!(
                "Failed to parse response with parse_response_oaicompat: {}",
                e
            );
            LLMError::ProviderError(format!("Failed to parse response: {}", e))
        })?;

        log::debug!("Parsed JSON: {}", parsed_json);

        let parsed: Value = serde_json::from_str(&parsed_json).map_err(|e| {
            LLMError::ProviderError(format!("Failed to deserialize parsed response: {}", e))
        })?;

        let raw_content = parsed
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // The C++ parser may have already extracted reasoning_content.
        // If so, use it directly. Otherwise, use extract_thinking as fallback.
        let reasoning_content = parsed
            .get("reasoning_content")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());

        let (thinking, content) = if reasoning_content.is_some() {
            (reasoning_content, raw_content)
        } else {
            querymt::chat::extract_thinking(&raw_content)
        };

        let tool_calls = parsed
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                if arr.is_empty() {
                    None
                } else {
                    serde_json::from_value(Value::Array(arr.clone())).ok()
                }
            });

        let finish_reason = if tool_calls.is_some() {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        };

        log::debug!(
            "Parsed response: content_len={}, thinking={}, tool_calls={}, finish_reason={:?}",
            content.len(),
            thinking.as_ref().map(|t| t.len()).unwrap_or(0),
            tool_calls
                .as_ref()
                .map(|tc: &Vec<querymt::ToolCall>| tc.len())
                .unwrap_or(0),
            finish_reason
        );

        Ok((content, thinking, tool_calls, finish_reason))
    }

    /// Generate text with streaming and grammar-constrained sampling for tool calls.
    /// Returns (Usage, has_tool_calls) where has_tool_calls indicates if tool calls were made.
    fn generate_streaming_with_tools(
        &self,
        result: &ChatTemplateResult,
        max_tokens: u32,
        temperature: Option<f32>,
        tx: &mpsc::UnboundedSender<Result<querymt::chat::StreamChunk, LLMError>>,
    ) -> Result<(Usage, bool), LLMError> {
        let backend = llama_backend()?;
        let add_bos = self.cfg.add_bos.unwrap_or(true);
        let tokens = self
            .model
            .str_to_token(
                &result.prompt,
                if add_bos {
                    AddBos::Always
                } else {
                    AddBos::Never
                },
            )
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;

        if tokens.is_empty() {
            return Err(LLMError::InvalidRequest(
                "Prompt tokenization resulted in an empty sequence".into(),
            ));
        }

        log::debug!(
            "Streaming generation with tools: input_tokens={}, max_tokens={}",
            tokens.len(),
            max_tokens
        );

        if max_tokens == 0 {
            return Ok((
                Usage {
                    input_tokens: tokens.len() as u32,
                    output_tokens: 0,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning_tokens: 0,
                },
                false,
            ));
        }

        // Auto-size context when not explicitly configured (tool prompts are large)
        let n_ctx_needed = tokens.len() as u32 + max_tokens;
        let n_ctx = if let Some(configured_n_ctx) = self.cfg.n_ctx {
            configured_n_ctx
        } else {
            // Only allocate what we actually need; cap at the model's training
            // context to avoid GPU out-of-memory when n_ctx is not configured.
            n_ctx_needed.min(self.model.n_ctx_train())
        };

        let mut ctx_params = LlamaContextParams::default();
        let n_ctx = NonZeroU32::new(n_ctx)
            .ok_or_else(|| LLMError::InvalidRequest("n_ctx must be greater than zero".into()))?;
        ctx_params = ctx_params.with_n_ctx(Some(n_ctx));
        ctx_params = ctx_params.with_n_batch(n_ctx.get());
        if let Some(n_threads) = self.cfg.n_threads {
            ctx_params = ctx_params.with_n_threads(n_threads);
        }
        if let Some(n_threads_batch) = self.cfg.n_threads_batch {
            ctx_params = ctx_params.with_n_threads_batch(n_threads_batch);
        }

        let mut ctx = self
            .model
            .new_context(&*backend, ctx_params)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;

        let n_ctx_total = ctx.n_ctx() as i32;
        let n_len_total = tokens.len() as i32 + max_tokens as i32;
        if n_len_total > n_ctx_total {
            return Err(LLMError::InvalidRequest(format!(
                "Prompt + max_tokens ({n_len_total}) exceeds context window ({n_ctx_total})"
            )));
        }

        let mut batch = LlamaBatch::new(n_ctx_total as usize, 1);

        let last_index = tokens.len().saturating_sub(1) as i32;
        for (i, token) in (0_i32..).zip(tokens.iter().copied()) {
            let is_last = i == last_index;
            batch
                .add(token, i, &[0], is_last)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        }

        ctx.decode(&mut batch)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;

        // Build preserved token set for special handling
        let mut preserved = HashSet::new();
        for token_str in &result.preserved_tokens {
            if let Ok(preserved_tokens) = self.model.str_to_token(token_str, AddBos::Never) {
                if preserved_tokens.len() == 1 {
                    preserved.insert(preserved_tokens[0]);
                }
            }
        }

        // Initialize streaming parser
        let mut stream_state = result.streaming_state_oaicompat().map_err(|e| {
            LLMError::ProviderError(format!("Failed to init streaming state: {}", e))
        })?;

        let mut sampler = self.build_tool_sampler(result, temperature);
        let mut n_cur = batch.n_tokens();
        let mut output_tokens = 0u32;
        let mut generated_text = String::new();
        let mut decoder = encoding_rs::UTF_8.new_decoder();

        // Track tool calls being assembled
        let mut tool_calls_in_progress: std::collections::HashMap<usize, (String, String, String)> =
            std::collections::HashMap::new();

        while n_cur < n_len_total {
            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            if self.model.is_eog_token(token) {
                break;
            }

            let special = preserved.contains(&token);
            let bytes = self
                .model
                .token_to_piece_bytes(token, 128, special, None)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
            let chunk = match self
                .model
                .token_to_piece(token, &mut decoder, special, None)
            {
                Ok(piece) => piece,
                Err(_) => String::from_utf8_lossy(&bytes).to_string(),
            };
            generated_text.push_str(&chunk);

            // Check additional stop sequences
            let stop_now = result
                .additional_stops
                .iter()
                .any(|stop| !stop.is_empty() && generated_text.ends_with(stop));

            // Update streaming parser
            match stream_state.update(&chunk, !stop_now) {
                Ok(deltas) => {
                    for delta_json in deltas {
                        // Parse each delta and emit appropriate StreamChunks
                        if let Ok(delta) = serde_json::from_str::<Value>(&delta_json) {
                            // Handle content delta
                            if let Some(content_delta) =
                                delta.get("content").and_then(|v| v.as_str())
                            {
                                if !content_delta.is_empty() {
                                    if tx
                                        .unbounded_send(Ok(querymt::chat::StreamChunk::Text(
                                            content_delta.to_string(),
                                        )))
                                        .is_err()
                                    {
                                        return Ok((
                                            Usage {
                                                input_tokens: tokens.len() as u32,
                                                output_tokens,
                                                cache_read: 0,
                                                cache_write: 0,
                                                reasoning_tokens: 0,
                                            },
                                            !tool_calls_in_progress.is_empty(),
                                        ));
                                    }
                                }
                            }

                            // Handle reasoning_content delta (thinking)
                            if let Some(reasoning_delta) =
                                delta.get("reasoning_content").and_then(|v| v.as_str())
                            {
                                if !reasoning_delta.is_empty() {
                                    if tx
                                        .unbounded_send(Ok(querymt::chat::StreamChunk::Thinking(
                                            reasoning_delta.to_string(),
                                        )))
                                        .is_err()
                                    {
                                        return Ok((
                                            Usage {
                                                input_tokens: tokens.len() as u32,
                                                output_tokens,
                                                cache_read: 0,
                                                cache_write: 0,
                                                reasoning_tokens: 0,
                                            },
                                            !tool_calls_in_progress.is_empty(),
                                        ));
                                    }
                                }
                            }

                            // Handle tool call deltas - parse tool_calls array
                            if let Some(tool_calls_arr) =
                                delta.get("tool_calls").and_then(|v| v.as_array())
                            {
                                for tc in tool_calls_arr {
                                    let index =
                                        tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0)
                                            as usize;

                                    // Check if this is a new tool call (has id and name)
                                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                        let name = tc
                                            .get("function")
                                            .and_then(|f| f.get("name"))
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");

                                        tool_calls_in_progress.insert(
                                            index,
                                            (id.to_string(), name.to_string(), String::new()),
                                        );

                                        if tx
                                            .unbounded_send(Ok(
                                                querymt::chat::StreamChunk::ToolUseStart {
                                                    index,
                                                    id: id.to_string(),
                                                    name: name.to_string(),
                                                },
                                            ))
                                            .is_err()
                                        {
                                            return Ok((
                                                Usage {
                                                    input_tokens: tokens.len() as u32,
                                                    output_tokens,
                                                    cache_read: 0,
                                                    cache_write: 0,
                                                    reasoning_tokens: 0,
                                                },
                                                !tool_calls_in_progress.is_empty(),
                                            ));
                                        }
                                    }

                                    // Always check for arguments delta
                                    if let Some(args) = tc
                                        .get("function")
                                        .and_then(|f| f.get("arguments"))
                                        .and_then(|v| v.as_str())
                                    {
                                        if !args.is_empty() {
                                            if let Some(entry) =
                                                tool_calls_in_progress.get_mut(&index)
                                            {
                                                entry.2.push_str(args);
                                            }

                                            if tx
                                                .unbounded_send(Ok(
                                                    querymt::chat::StreamChunk::ToolUseInputDelta {
                                                        index,
                                                        partial_json: args.to_string(),
                                                    },
                                                ))
                                                .is_err()
                                            {
                                                return Ok((
                                                    Usage {
                                                        input_tokens: tokens.len() as u32,
                                                        output_tokens,
                                                        cache_read: 0,
                                                        cache_write: 0,
                                                        reasoning_tokens: 0,
                                                    },
                                                    !tool_calls_in_progress.is_empty(),
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.unbounded_send(Err(LLMError::ProviderError(format!(
                        "Streaming parse error: {}",
                        e
                    ))));
                    return Err(LLMError::ProviderError(format!(
                        "Streaming parse error: {}",
                        e
                    )));
                }
            }

            if stop_now {
                break;
            }

            batch.clear();
            batch
                .add(token, n_cur, &[0], true)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
            n_cur += 1;
            output_tokens += 1;

            ctx.decode(&mut batch)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        }

        // Trim matched stop sequences
        for stop in &result.additional_stops {
            if !stop.is_empty() && generated_text.ends_with(stop) {
                let new_len = generated_text.len().saturating_sub(stop.len());
                generated_text.truncate(new_len);
                break;
            }
        }

        // Parse final response to get complete tool calls
        let (_, _, tool_calls, _) = Self::parse_tool_response(result, &generated_text)?;

        // Emit ToolUseComplete for each tool call
        let has_tool_calls = if let Some(calls) = tool_calls {
            for (index, call) in calls.into_iter().enumerate() {
                if tx
                    .unbounded_send(Ok(querymt::chat::StreamChunk::ToolUseComplete {
                        index,
                        tool_call: call,
                    }))
                    .is_err()
                {
                    break;
                }
            }
            true
        } else {
            false
        };

        Ok((
            Usage {
                input_tokens: tokens.len() as u32,
                output_tokens,
                cache_read: 0,
                cache_write: 0,
                reasoning_tokens: 0,
            },
            has_tool_calls,
        ))
    }

    fn build_prompt_with(
        &self,
        messages: &[ChatMessage],
        use_chat_template: bool,
    ) -> Result<(String, bool), LLMError> {
        for msg in messages {
            if !matches!(msg.message_type, MessageType::Text) {
                return Err(LLMError::InvalidRequest(
                    "Only text chat messages are supported by llama.cpp provider".into(),
                ));
            }
        }

        if !use_chat_template {
            let prompt = self.build_raw_prompt(messages)?;
            return Ok((prompt, false));
        }

        let mut chat_messages = Vec::with_capacity(messages.len() + 1);
        if !self.cfg.system.is_empty() {
            let system = self.cfg.system.join("\n\n");
            chat_messages.push(
                LlamaChatMessage::new("system".to_string(), system)
                    .map_err(|e| LLMError::InvalidRequest(e.to_string()))?,
            );
        }

        for msg in messages {
            let role = match msg.role {
                ChatRole::User => "user",
                ChatRole::Assistant => "assistant",
            };
            chat_messages.push(
                LlamaChatMessage::new(role.to_string(), msg.content.clone())
                    .map_err(|e| LLMError::InvalidRequest(e.to_string()))?,
            );
        }

        if let Ok(template) = self.model.chat_template(self.cfg.chat_template.as_deref()) {
            if let Ok(prompt) = self
                .model
                .apply_chat_template(&template, &chat_messages, true)
            {
                return Ok((prompt, true));
            }
        }

        let prompt = self.build_raw_prompt(messages)?;
        Ok((prompt, false))
    }

    fn build_prompt(&self, messages: &[ChatMessage]) -> Result<(String, bool), LLMError> {
        let use_chat_template = self.cfg.use_chat_template.unwrap_or(true);
        self.build_prompt_with(messages, use_chat_template)
    }

    fn build_prompt_candidates(&self, messages: &[ChatMessage]) -> Result<Vec<String>, LLMError> {
        let (prompt, used_chat_template) = self.build_prompt(messages)?;
        let mut prompts = vec![prompt];

        if used_chat_template && self.cfg.use_chat_template.is_none() {
            let (fallback_prompt, _) = self.build_prompt_with(messages, false)?;
            if !prompts.contains(&fallback_prompt) {
                prompts.push(fallback_prompt);
            }
        }

        let raw_prompt = self.build_raw_prompt(messages)?;
        if !prompts.contains(&raw_prompt) {
            prompts.push(raw_prompt);
        }

        Ok(prompts)
    }

    fn build_raw_prompt(&self, messages: &[ChatMessage]) -> Result<String, LLMError> {
        for msg in messages {
            if !matches!(msg.message_type, MessageType::Text) {
                return Err(LLMError::InvalidRequest(
                    "Only text chat messages are supported by llama.cpp provider".into(),
                ));
            }
        }

        let mut prompt = String::new();
        if !self.cfg.system.is_empty() {
            prompt.push_str(&self.cfg.system.join("\n\n"));
            prompt.push_str("\n\n");
        }
        for (idx, msg) in messages.iter().enumerate() {
            prompt.push_str(&msg.content);
            if idx + 1 < messages.len() {
                prompt.push_str("\n\n");
            }
        }
        Ok(prompt)
    }

    fn build_sampler(&self, temperature: Option<f32>) -> LlamaSampler {
        let mut samplers = Vec::new();
        let temp = temperature.or(self.cfg.temperature);

        if let Some(temp) = temp {
            if temp > 0.0 {
                samplers.push(LlamaSampler::temp(temp));
            }
        }
        if let Some(top_p) = self.cfg.top_p {
            samplers.push(LlamaSampler::top_p(top_p, 1));
        }
        if let Some(top_k) = self.cfg.top_k {
            samplers.push(LlamaSampler::top_k(top_k as i32));
        }

        let use_sampling =
            temp.map_or(false, |t| t > 0.0) || self.cfg.top_p.is_some() || self.cfg.top_k.is_some();
        let seed = self.cfg.seed.unwrap_or(1234);
        if use_sampling {
            samplers.push(LlamaSampler::dist(seed));
        } else {
            samplers.push(LlamaSampler::dist(seed));
            samplers.push(LlamaSampler::greedy());
        }

        LlamaSampler::chain_simple(samplers)
    }

    fn generate(
        &self,
        prompt: &str,
        max_tokens: u32,
        temperature: Option<f32>,
    ) -> Result<GeneratedText, LLMError> {
        let backend = llama_backend()?;
        let add_bos = self.cfg.add_bos.unwrap_or(true);
        let tokens = self
            .model
            .str_to_token(
                prompt,
                if add_bos {
                    AddBos::Always
                } else {
                    AddBos::Never
                },
            )
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        if tokens.is_empty() {
            return Err(LLMError::InvalidRequest(
                "Prompt tokenization resulted in an empty sequence".into(),
            ));
        }
        if max_tokens == 0 {
            return Ok(GeneratedText {
                text: String::new(),
                usage: Usage {
                    input_tokens: tokens.len() as u32,
                    output_tokens: 0,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning_tokens: 0,
                },
            });
        }

        let mut ctx_params = LlamaContextParams::default();
        if let Some(n_ctx) = self.cfg.n_ctx {
            let n_ctx = NonZeroU32::new(n_ctx).ok_or_else(|| {
                LLMError::InvalidRequest("n_ctx must be greater than zero".into())
            })?;
            ctx_params = ctx_params.with_n_ctx(Some(n_ctx));
            // Set n_batch to match n_ctx so large prompts (e.g. with tools) can be decoded
            ctx_params = ctx_params.with_n_batch(n_ctx.get());
        }
        if let Some(n_threads) = self.cfg.n_threads {
            ctx_params = ctx_params.with_n_threads(n_threads);
        }
        if let Some(n_threads_batch) = self.cfg.n_threads_batch {
            ctx_params = ctx_params.with_n_threads_batch(n_threads_batch);
        }

        let mut ctx = self
            .model
            .new_context(&*backend, ctx_params)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;

        let n_ctx_total = ctx.n_ctx() as i32;
        let n_len_total = tokens.len() as i32 + max_tokens as i32;
        if n_len_total > n_ctx_total {
            return Err(LLMError::InvalidRequest(format!(
                "Prompt + max_tokens ({n_len_total}) exceeds context window ({n_ctx_total})"
            )));
        }

        // Allocate batch to fit all prompt tokens (n_batch was set to n_ctx above)
        let mut batch = LlamaBatch::new(n_ctx_total as usize, 1);

        let last_index = tokens.len().saturating_sub(1) as i32;
        for (i, token) in (0_i32..).zip(tokens.iter().copied()) {
            let is_last = i == last_index;
            batch
                .add(token, i, &[0], is_last)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        }

        ctx.decode(&mut batch)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;

        let mut sampler = self.build_sampler(temperature);
        let allow_fallback = temperature.is_none()
            && self.cfg.temperature.is_none()
            && self.cfg.top_p.is_none()
            && self.cfg.top_k.is_none();
        let mut fallback_used = false;

        let mut n_cur = batch.n_tokens();
        let mut output_tokens = 0u32;
        let mut output = String::new();
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        while n_cur < n_len_total {
            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            if self.model.is_eog_token(token) {
                if output_tokens == 0 && allow_fallback && !fallback_used {
                    sampler = self.build_fallback_sampler();
                    fallback_used = true;
                    continue;
                }
                break;
            }
            sampler.accept(token);

            let bytes = self
                .model
                .token_to_piece_bytes(token, 128, true, None)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
            let chunk = match self.model.token_to_piece(token, &mut decoder, true, None) {
                Ok(piece) => piece,
                Err(_) => String::from_utf8_lossy(&bytes).to_string(),
            };
            output.push_str(&chunk);

            batch.clear();
            batch
                .add(token, n_cur, &[0], true)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
            n_cur += 1;
            output_tokens += 1;

            ctx.decode(&mut batch)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        }

        Ok(GeneratedText {
            text: output,
            usage: Usage {
                input_tokens: tokens.len() as u32,
                output_tokens,
                cache_read: 0,
                cache_write: 0,
                reasoning_tokens: 0,
            },
        })
    }

    fn generate_streaming(
        &self,
        prompt: &str,
        max_tokens: u32,
        temperature: Option<f32>,
        tx: &mpsc::UnboundedSender<Result<querymt::chat::StreamChunk, LLMError>>,
    ) -> Result<Usage, LLMError> {
        let backend = llama_backend()?;
        let add_bos = self.cfg.add_bos.unwrap_or(true);
        let tokens = self
            .model
            .str_to_token(
                prompt,
                if add_bos {
                    AddBos::Always
                } else {
                    AddBos::Never
                },
            )
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        if tokens.is_empty() {
            return Err(LLMError::InvalidRequest(
                "Prompt tokenization resulted in an empty sequence".into(),
            ));
        }
        if max_tokens == 0 {
            return Ok(Usage {
                input_tokens: tokens.len() as u32,
                output_tokens: 0,
                cache_read: 0,
                cache_write: 0,
                reasoning_tokens: 0,
            });
        }

        let mut ctx_params = LlamaContextParams::default();
        if let Some(n_ctx) = self.cfg.n_ctx {
            let n_ctx = NonZeroU32::new(n_ctx).ok_or_else(|| {
                LLMError::InvalidRequest("n_ctx must be greater than zero".into())
            })?;
            ctx_params = ctx_params.with_n_ctx(Some(n_ctx));
            // Set n_batch to match n_ctx so large prompts (e.g. with tools) can be decoded
            ctx_params = ctx_params.with_n_batch(n_ctx.get());
        }
        if let Some(n_threads) = self.cfg.n_threads {
            ctx_params = ctx_params.with_n_threads(n_threads);
        }
        if let Some(n_threads_batch) = self.cfg.n_threads_batch {
            ctx_params = ctx_params.with_n_threads_batch(n_threads_batch);
        }

        let mut ctx = self
            .model
            .new_context(&*backend, ctx_params)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;

        let n_ctx_total = ctx.n_ctx() as i32;
        let n_len_total = tokens.len() as i32 + max_tokens as i32;
        if n_len_total > n_ctx_total {
            return Err(LLMError::InvalidRequest(format!(
                "Prompt + max_tokens ({n_len_total}) exceeds context window ({n_ctx_total})"
            )));
        }

        // Allocate batch to fit all prompt tokens (n_batch was set to n_ctx above)
        let mut batch = LlamaBatch::new(n_ctx_total as usize, 1);

        let last_index = tokens.len().saturating_sub(1) as i32;
        for (i, token) in (0_i32..).zip(tokens.iter().copied()) {
            let is_last = i == last_index;
            batch
                .add(token, i, &[0], is_last)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        }

        ctx.decode(&mut batch)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;

        let mut sampler = self.build_sampler(temperature);
        let allow_fallback = temperature.is_none()
            && self.cfg.temperature.is_none()
            && self.cfg.top_p.is_none()
            && self.cfg.top_k.is_none();
        let mut fallback_used = false;

        let mut n_cur = batch.n_tokens();
        let mut output_tokens = 0u32;
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        while n_cur < n_len_total {
            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            if self.model.is_eog_token(token) {
                if output_tokens == 0 && allow_fallback && !fallback_used {
                    sampler = self.build_fallback_sampler();
                    fallback_used = true;
                    continue;
                }
                break;
            }
            sampler.accept(token);

            let bytes = self
                .model
                .token_to_piece_bytes(token, 128, true, None)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
            let chunk = match self.model.token_to_piece(token, &mut decoder, true, None) {
                Ok(piece) => piece,
                Err(_) => String::from_utf8_lossy(&bytes).to_string(),
            };
            if !chunk.is_empty() {
                if tx
                    .unbounded_send(Ok(querymt::chat::StreamChunk::Text(chunk)))
                    .is_err()
                {
                    break;
                }
            }

            batch.clear();
            batch
                .add(token, n_cur, &[0], true)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
            n_cur += 1;
            output_tokens += 1;

            ctx.decode(&mut batch)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        }

        Ok(Usage {
            input_tokens: tokens.len() as u32,
            output_tokens,
            cache_read: 0,
            cache_write: 0,
            reasoning_tokens: 0,
        })
    }
}

#[async_trait]
impl ChatProvider for LlamaCppProvider {
    fn supports_streaming(&self) -> bool {
        true
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        let max_tokens = self.cfg.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

        // If tools are provided and not empty, use tool-aware generation
        if let Some(tools) = tools {
            if !tools.is_empty() {
                let template_result = self.apply_template_with_tools(messages, tools)?;
                let generated = self.generate_with_tools(&template_result, max_tokens, None)?;
                let (content, thinking, tool_calls, finish_reason) =
                    Self::parse_tool_response(&template_result, &generated.text)?;

                return Ok(Box::new(LlamaCppChatResponse {
                    text: content,
                    thinking,
                    tool_calls,
                    finish_reason,
                    usage: generated.usage,
                }));
            }
        }

        // Fall back to standard generation without tools
        let (prompt, used_chat_template) = self.build_prompt(messages)?;
        let mut generated = self.generate(&prompt, max_tokens, None)?;
        if generated.text.trim().is_empty() {
            if used_chat_template && self.cfg.use_chat_template.is_none() {
                let (fallback_prompt, _) = self.build_prompt_with(messages, false)?;
                generated = self.generate(&fallback_prompt, max_tokens, None)?;
            }
        }
        if generated.text.trim().is_empty() {
            let raw_prompt = self.build_raw_prompt(messages)?;
            generated = self.generate(&raw_prompt, max_tokens, None)?;
        }
        let (thinking, clean_text) = querymt::chat::extract_thinking(&generated.text);
        Ok(Box::new(LlamaCppChatResponse {
            text: clean_text,
            thinking,
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: generated.usage,
        }))
    }

    async fn chat_stream_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<
        std::pin::Pin<Box<dyn Stream<Item = Result<querymt::chat::StreamChunk, LLMError>> + Send>>,
        LLMError,
    > {
        let max_tokens = self.cfg.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
        let (tx, rx) = mpsc::unbounded();

        // If tools are provided and not empty, use tool-aware streaming
        if let Some(tools) = tools {
            if !tools.is_empty() {
                let template_result = self.apply_template_with_tools(messages, tools)?;
                let cfg = self.cfg.clone();
                let model = Arc::clone(&self.model);

                thread::spawn(move || {
                    let provider = LlamaCppProvider { model, cfg };

                    match provider.generate_streaming_with_tools(
                        &template_result,
                        max_tokens,
                        None,
                        &tx,
                    ) {
                        Ok((usage, has_tool_calls)) => {
                            let _ = tx.unbounded_send(Ok(querymt::chat::StreamChunk::Usage(usage)));
                            let stop_reason = if has_tool_calls {
                                "tool_use"
                            } else {
                                "end_turn"
                            };
                            let _ = tx.unbounded_send(Ok(querymt::chat::StreamChunk::Done {
                                stop_reason: stop_reason.to_string(),
                            }));
                        }
                        Err(err) => {
                            let _ = tx.unbounded_send(Err(err));
                        }
                    }
                });

                return Ok(Box::pin(rx));
            }
        }

        // Fall back to standard streaming without tools
        let prompts = self.build_prompt_candidates(messages)?;
        let cfg = self.cfg.clone();
        let model = Arc::clone(&self.model);

        thread::spawn(move || {
            let provider = LlamaCppProvider { model, cfg };
            let mut final_usage = None;
            for (idx, prompt) in prompts.iter().enumerate() {
                match provider.generate_streaming(prompt, max_tokens, None, &tx) {
                    Ok(usage) => {
                        let should_fallback = usage.output_tokens == 0 && idx + 1 < prompts.len();
                        if should_fallback {
                            continue;
                        }
                        final_usage = Some(usage);
                        break;
                    }
                    Err(err) => {
                        let _ = tx.unbounded_send(Err(err));
                        return;
                    }
                }
            }

            if let Some(usage) = final_usage {
                let _ = tx.unbounded_send(Ok(querymt::chat::StreamChunk::Usage(usage)));
                let _ = tx.unbounded_send(Ok(querymt::chat::StreamChunk::Done {
                    stop_reason: "end_turn".to_string(),
                }));
            }
        });

        Ok(Box::pin(rx))
    }
}

#[async_trait]
impl CompletionProvider for LlamaCppProvider {
    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        if req.suffix.is_some() {
            return Err(LLMError::NotImplemented(
                "Suffix completion is not supported by llama.cpp provider".into(),
            ));
        }

        let max_tokens = req
            .max_tokens
            .or(self.cfg.max_tokens)
            .unwrap_or(DEFAULT_MAX_TOKENS);
        let generated = self.generate(&req.prompt, max_tokens, req.temperature)?;
        Ok(CompletionResponse {
            text: generated.text,
        })
    }
}

#[async_trait]
impl EmbeddingProvider for LlamaCppProvider {
    async fn embed(&self, _input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        Err(LLMError::NotImplemented(
            "Embeddings are not supported by llama.cpp provider".into(),
        ))
    }
}

impl LLMProvider for LlamaCppProvider {}

struct LlamaCppFactory;

impl LLMProviderFactory for LlamaCppFactory {
    fn name(&self) -> &str {
        "llama_cpp"
    }

    fn config_schema(&self) -> String {
        let schema = schema_for!(LlamaCppConfig);
        serde_json::to_string(&schema.schema)
            .expect("LlamaCppConfig schema should always serialize")
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn LLMProvider>, LLMError> {
        let cfg: LlamaCppConfig = serde_json::from_str(cfg)?;
        let provider = LlamaCppProvider::new(cfg)?;
        Ok(Box::new(provider))
    }

    fn list_models<'a>(&'a self, cfg: &str) -> Fut<'a, Result<Vec<String>, LLMError>> {
        let cfg = cfg.to_string();
        Box::pin(async move {
            let cfg: LlamaCppConfig = serde_json::from_str(&cfg)?;
            let model_name = cfg.model.clone().or_else(|| {
                Path::new(&cfg.model_path)
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
            });
            Ok(vec![model_name.unwrap_or(cfg.model_path)])
        })
    }
}

#[cfg(feature = "native")]
#[unsafe(no_mangle)]
pub extern "C" fn plugin_factory() -> *mut dyn LLMProviderFactory {
    Box::into_raw(Box::new(LlamaCppFactory)) as *mut _
}

/// Initialize logging from the host process.
///
/// This function is called by the host after loading the plugin via dlopen.
/// It sets up a logger that forwards all `log` crate calls from this plugin
/// back to the host's logger, enabling `RUST_LOG` filtering to work for the plugin.
///
/// # Safety
///
/// This function is unsafe because:
/// - The `callback` function pointer must remain valid for the lifetime of the plugin
/// - This should only be called once per plugin load (the host ensures this)
/// - The callback must be thread-safe
#[cfg(feature = "native")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_init_logging(
    callback: querymt::plugin::LogCallbackFn,
    max_level: usize,
) {
    querymt::plugin::plugin_log::init_from_host(callback, max_level);
}
