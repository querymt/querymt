use async_trait::async_trait;
use futures::channel::mpsc;
use futures::Stream;
use hf_hub::api::sync::ApiBuilder;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaModel, Special};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::{send_logs_to_tracing, LogOptions};
use querymt::chat::{
    ChatMessage, ChatProvider, ChatResponse, ChatRole, FinishReason, MessageType, Tool,
};
use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
use querymt::embedding::EmbeddingProvider;
use querymt::error::LLMError;
use querymt::plugin::{Fut, LLMProviderFactory};
use querymt::{LLMProvider, Usage};
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

const DEFAULT_MAX_TOKENS: u32 = 256;
const DEFAULT_BATCH: u32 = 512;

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
    pub system: Option<String>,
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

    fn tool_calls(&self) -> Option<Vec<querymt::ToolCall>> {
        None
    }

    fn usage(&self) -> Option<Usage> {
        Some(self.usage.clone())
    }

    fn finish_reason(&self) -> Option<FinishReason> {
        todo!()
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
    fn resolve_model_path(raw: &str) -> Result<PathBuf, LLMError> {
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
        let api = ApiBuilder::new()
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
        let model_path = Self::resolve_model_path(&cfg.model_path)?;
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
        if let Some(system) = &self.cfg.system {
            chat_messages.push(
                LlamaChatMessage::new("system".to_string(), system.clone())
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
        if let Some(system) = &self.cfg.system {
            prompt.push_str(system);
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

        let batch_tokens = self.cfg.n_batch.unwrap_or(DEFAULT_BATCH);
        let mut batch = LlamaBatch::new(batch_tokens as usize, 1);

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
                .token_to_bytes(token, Special::Tokenize)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
            let chunk = match self.model.token_to_str(token, Special::Tokenize) {
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

        let batch_tokens = self.cfg.n_batch.unwrap_or(DEFAULT_BATCH);
        let mut batch = LlamaBatch::new(batch_tokens as usize, 1);

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
                .token_to_bytes(token, Special::Tokenize)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
            let chunk = match self.model.token_to_str(token, Special::Tokenize) {
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
        if tools.is_some() {
            return Err(LLMError::NotImplemented(
                "Tool calling is not supported by llama.cpp provider".into(),
            ));
        }

        let max_tokens = self.cfg.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
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
        Ok(Box::new(LlamaCppChatResponse {
            text: generated.text,
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
        if tools.is_some() {
            return Err(LLMError::NotImplemented(
                "Tool calling is not supported by llama.cpp provider".into(),
            ));
        }

        let max_tokens = self.cfg.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
        let prompts = self.build_prompt_candidates(messages)?;
        let (tx, rx) = mpsc::unbounded();
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
        serde_json::to_string(&schema.schema).expect("LlamaCppConfig schema should always serialize")
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
