use crate::backend::{install_abort_callback, llama_backend};
use crate::config::{DEFAULT_MAX_TOKENS, LlamaCppConfig, LlamaCppLogMode};
use crate::context::estimate_context_memory;
use crate::generation::{
    build_prompt, build_prompt_with, build_raw_prompt, generate, generate_streaming_with_thinking,
};
use crate::memory::MemoryEstimate;
use crate::multimodal::MultimodalContext;
use crate::response::LlamaCppChatResponse;
use crate::tools::{
    apply_template_for_thinking, apply_template_with_tools, generate_streaming_with_tools,
    generate_with_tools, parse_tool_response,
};
use async_trait::async_trait;
use futures::Stream;
use futures::channel::mpsc;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::{LogOptions, send_logs_to_tracing};
use querymt::LLMProvider;
use querymt::chat::{ChatMessage, ChatProvider, ChatResponse, Tool};
use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
use querymt::embedding::EmbeddingProvider;
use querymt::error::LLMError;
use querymt_provider_common::{
    HfFileRef, ModelRef, ModelRefError, download_hf_file_sync, parse_model_ref, terminal_progress,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

/// Cache key for model loading — only params that affect `LlamaModel::load_from_file`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelCacheKey {
    /// Resolved absolute path to the GGUF file.
    pub model_path: String,
    /// Number of GPU layers (affects Metal/CUDA offloading).
    pub n_gpu_layers: Option<u32>,
    /// Resolved MTP sidecar path, when configured.
    pub mtp_model_path: Option<String>,
    /// Sidecar GPU layers.
    pub mtp_n_gpu_layers: Option<u32>,
}

/// A cached model + multimodal context, shared across provider instances.
pub(crate) struct CachedModel {
    pub key: ModelCacheKey,
    pub model: Arc<LlamaModel>,
    pub multimodal: Option<Arc<MultimodalContext>>,
    pub mtp_model: Option<Arc<LlamaModel>>,
}

/// The main llama.cpp provider.
pub(crate) struct LlamaCppProvider {
    pub(crate) model: Arc<LlamaModel>,
    pub(crate) cfg: LlamaCppConfig,
    pub(crate) multimodal: Option<Arc<MultimodalContext>>,
    pub(crate) mtp_model: Option<Arc<LlamaModel>>,
}

impl LlamaCppProvider {
    /// Resolve a model path, potentially downloading from Hugging Face Hub.
    fn resolve_model_path(raw: &str) -> Result<PathBuf, LLMError> {
        let model_ref = parse_model_ref(raw).map_err(Self::map_model_ref_error)?;
        match model_ref {
            ModelRef::LocalPath(path) => Ok(path),
            ModelRef::Hf(model) => {
                let file = HfFileRef::from(&model);
                download_hf_file_sync(&file, terminal_progress(&model.file))
                    .map_err(Self::map_model_ref_error)
            }
            ModelRef::HfRepo(repo) => Err(LLMError::InvalidRequest(format!(
                "llama_cpp model must include a selector for Hugging Face repos: {repo}:<selector>"
            ))),
        }
    }

    fn map_model_ref_error(err: ModelRefError) -> LLMError {
        match err {
            ModelRefError::Invalid(msg) => LLMError::InvalidRequest(msg),
            ModelRefError::Download(msg) => LLMError::ProviderError(msg),
        }
    }

    pub(crate) fn new(cfg: LlamaCppConfig) -> Result<Self, LLMError> {
        install_abort_callback();

        let mut backend = llama_backend()?;
        let log_mode = cfg.log.unwrap_or(LlamaCppLogMode::Off);
        match log_mode {
            LlamaCppLogMode::Stderr => {}
            LlamaCppLogMode::Tracing => send_logs_to_tracing(LogOptions::default()),
            LlamaCppLogMode::Off => backend.void_logs(),
        }
        if let Some(speculative) = &cfg.speculative {
            speculative.params().map_err(LLMError::InvalidRequest)?;
        }
        let model_path = Self::resolve_model_path(&cfg.model)?;
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
        let model = Arc::new(
            LlamaModel::load_from_file(&*backend, &model_path, &params)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?,
        );

        let mtp_model =
            if let Some(sidecar) = cfg.speculative.as_ref().and_then(|m| m.model.as_deref()) {
                let sidecar_path = Self::resolve_model_path(sidecar)?;
                let mut params = LlamaModelParams::default();
                if let Some(n) = cfg
                    .speculative
                    .as_ref()
                    .and_then(|m| m.n_gpu_layers)
                    .or(cfg.n_gpu_layers)
                {
                    params = params.with_n_gpu_layers(n);
                }
                Some(Arc::new(
                    LlamaModel::load_from_file(&*backend, &sidecar_path, &params).map_err(|e| {
                        LLMError::ProviderError(format!("Failed to load MTP sidecar: {e}"))
                    })?,
                ))
            } else {
                None
            };

        let model_hf_repo = match parse_model_ref(&cfg.model) {
            Ok(ModelRef::Hf(hf_ref)) => Some(hf_ref.repo),
            _ => None,
        };
        let multimodal =
            MultimodalContext::new(&model, &cfg, model_hf_repo.as_deref())?.map(Arc::new);

        let provider = Self {
            model,
            cfg,
            multimodal,
            mtp_model,
        };
        Self::log_memory_advisory(&provider);
        Ok(provider)
    }

    /// Build a provider, reusing a cached model if the cache key matches.
    ///
    /// Model loading (`LlamaModel::load_from_file`) is the expensive operation.
    /// The cache stores the loaded `Arc<LlamaModel>` and `Arc<MultimodalContext>`.
    /// Each call returns a cheap provider wrapper that shares the cached model
    /// but carries its own per-request config (system, temperature, etc.).
    pub(crate) fn new_with_cache(
        cfg: LlamaCppConfig,
        cache: &std::sync::Mutex<Option<CachedModel>>,
    ) -> Result<Self, LLMError> {
        install_abort_callback();

        let mut backend = llama_backend()?;
        match cfg.log.unwrap_or(LlamaCppLogMode::Off) {
            LlamaCppLogMode::Stderr => {}
            LlamaCppLogMode::Tracing => send_logs_to_tracing(LogOptions::default()),
            LlamaCppLogMode::Off => backend.void_logs(),
        }
        if let Some(speculative) = &cfg.speculative {
            speculative.params().map_err(LLMError::InvalidRequest)?;
        }

        let model_path = Self::resolve_model_path(&cfg.model)?;
        let mtp_model_path = cfg
            .speculative
            .as_ref()
            .and_then(|m| m.model.as_deref())
            .map(Self::resolve_model_path)
            .transpose()?;
        let key = ModelCacheKey {
            model_path: model_path.to_string_lossy().to_string(),
            n_gpu_layers: cfg.n_gpu_layers,
            mtp_model_path: mtp_model_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            mtp_n_gpu_layers: cfg
                .speculative
                .as_ref()
                .and_then(|m| m.n_gpu_layers)
                .or(cfg.n_gpu_layers),
        };

        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.as_ref() {
            if cached.key == key {
                return Ok(Self {
                    model: Arc::clone(&cached.model),
                    cfg,
                    multimodal: cached.multimodal.as_ref().map(Arc::clone),
                    mtp_model: cached.mtp_model.as_ref().map(Arc::clone),
                });
            }
        }
        drop(guard);

        let model_path = Path::new(&key.model_path);
        if !model_path.exists() {
            return Err(LLMError::InvalidRequest(format!(
                "Model path does not exist: {}",
                model_path.display()
            )));
        }
        let mut params = LlamaModelParams::default();
        if let Some(n) = cfg.n_gpu_layers {
            params = params.with_n_gpu_layers(n);
        }
        let model = Arc::new(
            LlamaModel::load_from_file(&backend, model_path, &params)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?,
        );

        let mtp_model = if let Some(path) = &key.mtp_model_path {
            let mut params = LlamaModelParams::default();
            if let Some(n) = key.mtp_n_gpu_layers {
                params = params.with_n_gpu_layers(n);
            }
            Some(Arc::new(
                LlamaModel::load_from_file(&backend, Path::new(path), &params).map_err(|e| {
                    LLMError::ProviderError(format!("Failed to load MTP sidecar: {e}"))
                })?,
            ))
        } else {
            None
        };

        let model_hf_repo = match parse_model_ref(&cfg.model) {
            Ok(ModelRef::Hf(hf_ref)) => Some(hf_ref.repo),
            _ => None,
        };
        let multimodal =
            MultimodalContext::new(&model, &cfg, model_hf_repo.as_deref())?.map(Arc::new);

        let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(CachedModel {
            key,
            model: Arc::clone(&model),
            multimodal: multimodal.as_ref().map(Arc::clone),
            mtp_model: mtp_model.as_ref().map(Arc::clone),
        });

        let provider = Self {
            model,
            cfg,
            multimodal,
            mtp_model,
        };
        Self::log_memory_advisory(&provider);
        Ok(provider)
    }

    /// Log advisory memory warnings at startup.
    fn log_memory_advisory(provider: &Self) {
        if let Some(n_ctx) = provider.cfg.n_ctx {
            let est = estimate_context_memory(&provider.model, &provider.cfg, n_ctx);
            log::info!(
                "Model loaded: {} layers, {} KV heads, {}. {}",
                provider.model.n_layer(),
                provider.model.n_head_kv(),
                if est.gpu_memory_bytes > 0 {
                    format!("GPU: {} ({:.1}GB)", est.gpu_name, est.gpu_gb())
                } else {
                    "GPU: unknown".to_string()
                },
                est.summary(),
            );
            if est.gpu_memory_bytes > 0 && est.total_bytes > est.gpu_memory_bytes {
                let suggestions = MemoryEstimate::suggestions(
                    n_ctx,
                    provider.cfg.kv_cache_type_k.is_some()
                        || provider.cfg.kv_cache_type_v.is_some(),
                    provider.cfg.flash_attention.is_some(),
                );
                log::warn!(
                    "Configured n_ctx={} may exceed available GPU memory. \
                     Estimated {:.1}GB needed but only {:.1}GB available on {}. \
                     This could cause a GPU error during inference.\n{}",
                    n_ctx,
                    est.total_gb(),
                    est.gpu_gb(),
                    est.gpu_name,
                    suggestions,
                );
            }
        }
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

        // Extract media from messages (empty vec if none)
        let media = crate::multimodal::extract_media(messages);

        // Validate: if images present but no multimodal support, error
        if !media.is_empty() && self.multimodal.is_none() {
            return Err(LLMError::InvalidRequest(
                "Images provided but model does not support vision. \
                 Please configure mmproj_path or use a vision-capable model."
                    .into(),
            ));
        }

        // Convert media to bitmaps (if multimodal context available)
        let bitmaps = if let Some(ref mm_ctx) = self.multimodal {
            media
                .iter()
                .map(|m| m.to_bitmap(&mm_ctx.ctx))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            vec![]
        };

        // Only route through MTMD when the prompt actually contains media.
        let active_multimodal = if bitmaps.is_empty() {
            None
        } else {
            self.multimodal.as_deref()
        };
        let media_marker = active_multimodal.map(|m| m.marker());

        // If tools are provided and not empty, use tool-aware generation
        if let Some(tools) = tools {
            if !tools.is_empty() {
                let template_result = apply_template_with_tools(
                    &self.model,
                    &self.cfg,
                    messages,
                    tools,
                    media_marker,
                )?;
                let generated = generate_with_tools(
                    &self.model,
                    &self.cfg,
                    self.mtp_model.as_ref(),
                    &template_result,
                    max_tokens,
                    None,
                    active_multimodal,
                    &bitmaps,
                )?;
                let (content, thinking, tool_calls, _) =
                    parse_tool_response(&template_result, &generated.text)?;
                let finish_reason = generated
                    .termination
                    .finish_reason(tool_calls.as_ref().is_some_and(|calls| !calls.is_empty()));

                return Ok(Box::new(LlamaCppChatResponse {
                    text: content,
                    thinking,
                    tool_calls,
                    finish_reason,
                    usage: generated.usage,
                }));
            }
        }

        // Structured output: use OAI-compat template so the schema is converted
        // to a GBNF grammar that constrains sampling to valid JSON.
        if self.cfg.json_schema.is_some() {
            let template_result =
                apply_template_for_thinking(&self.model, &self.cfg, messages, media_marker)?;
            let generated = generate_with_tools(
                &self.model,
                &self.cfg,
                self.mtp_model.as_ref(),
                &template_result,
                max_tokens,
                None,
                active_multimodal,
                &bitmaps,
            )?;
            let (content, thinking, _tool_calls, _) =
                parse_tool_response(&template_result, &generated.text)?;
            let finish_reason = generated.termination.finish_reason(false);
            return Ok(Box::new(LlamaCppChatResponse {
                text: content,
                thinking,
                tool_calls: None,
                finish_reason,
                usage: generated.usage,
            }));
        }

        // Standard generation (with or without images)
        let (prompt, used_chat_template) =
            build_prompt(&self.model, &self.cfg, messages, media_marker)?;

        // Call unified generate() with optional multimodal params
        let mut generated = generate(
            &self.model,
            &self.cfg,
            self.mtp_model.as_ref(),
            &prompt,
            max_tokens,
            None,
            active_multimodal,
            &bitmaps,
        )?;
        // Retry alternate prompt formats only when the model ended immediately.
        if generated.text.trim().is_empty()
            && generated.termination == crate::response::GenerationTermination::Eog
        {
            if used_chat_template && self.cfg.use_chat_template.is_none() {
                let (fallback_prompt, _) =
                    build_prompt_with(&self.model, &self.cfg, messages, false, media_marker)?;
                generated = generate(
                    &self.model,
                    &self.cfg,
                    self.mtp_model.as_ref(),
                    &fallback_prompt,
                    max_tokens,
                    None,
                    active_multimodal,
                    &bitmaps,
                )?;
            }
        }
        if generated.text.trim().is_empty()
            && generated.termination == crate::response::GenerationTermination::Eog
        {
            let raw_prompt = build_raw_prompt(&self.cfg, messages)?;
            generated = generate(
                &self.model,
                &self.cfg,
                self.mtp_model.as_ref(),
                &raw_prompt,
                max_tokens,
                None,
                active_multimodal,
                &bitmaps,
            )?;
        }
        let reasoning_format = crate::common_chat::ReasoningFormat::detect(&prompt);
        let parsed = crate::chat_format::parse_assistant_format_with_state(
            &generated.text,
            reasoning_format,
            false,
        );
        let clean_text = parsed.content;
        let thinking = parsed.thinking;
        Ok(Box::new(LlamaCppChatResponse {
            text: clean_text,
            thinking,
            tool_calls: None,
            finish_reason: generated.termination.finish_reason(false),
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

        // Extract media from messages
        let media = crate::multimodal::extract_media(messages);

        // Validate multimodal support
        if !media.is_empty() && self.multimodal.is_none() {
            return Err(LLMError::InvalidRequest(
                "Images provided but model does not support vision.".into(),
            ));
        }

        // Convert media to bitmaps
        let bitmaps = if let Some(ref mm_ctx) = self.multimodal {
            media
                .iter()
                .map(|m| m.to_bitmap(&mm_ctx.ctx))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            vec![]
        };

        let active_multimodal = if bitmaps.is_empty() {
            None
        } else {
            self.multimodal.as_deref()
        };
        let media_marker = active_multimodal.map(|m| m.marker());

        // If tools are provided and not empty, use tool-aware streaming
        if let Some(tools) = tools {
            if !tools.is_empty() {
                let template_result = apply_template_with_tools(
                    &self.model,
                    &self.cfg,
                    messages,
                    tools,
                    media_marker,
                )?;
                let cfg = self.cfg.clone();
                let model = Arc::clone(&self.model);
                let mtp_model = self.mtp_model.clone();
                let multimodal = if bitmaps.is_empty() {
                    None
                } else {
                    self.multimodal.clone()
                };

                thread::spawn(move || {
                    match generate_streaming_with_tools(
                        &model,
                        &cfg,
                        mtp_model.as_ref(),
                        &template_result,
                        max_tokens,
                        None,
                        &tx,
                        multimodal.as_deref(),
                        &bitmaps,
                    ) {
                        Ok((usage, termination, has_tool_calls)) => {
                            let _ = tx.unbounded_send(Ok(querymt::chat::StreamChunk::Usage(usage)));
                            let _ = tx.unbounded_send(Ok(querymt::chat::StreamChunk::Done {
                                finish_reason: termination.finish_reason(has_tool_calls),
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

        // No-tool streaming uses the same Rust-side template path as tool
        // streaming so thinking markers are routed to StreamChunk::Thinking.
        // Template failures are surfaced instead of silently degrading to raw
        // streaming, which would leak <think> tags to the UI.
        let thinking_template =
            apply_template_for_thinking(&self.model, &self.cfg, messages, media_marker)?;
        let cfg = self.cfg.clone();
        let model = Arc::clone(&self.model);
        let mtp_model = self.mtp_model.clone();
        let multimodal = if bitmaps.is_empty() {
            None
        } else {
            self.multimodal.clone()
        };

        thread::spawn(move || {
            match generate_streaming_with_thinking(
                &model,
                &cfg,
                mtp_model.as_ref(),
                &thinking_template,
                max_tokens,
                None,
                &tx,
                multimodal.as_deref(),
                &bitmaps,
            ) {
                Ok((usage, termination)) => {
                    let _ = tx.unbounded_send(Ok(querymt::chat::StreamChunk::Usage(usage)));
                    let _ = tx.unbounded_send(Ok(querymt::chat::StreamChunk::Done {
                        finish_reason: termination.finish_reason(false),
                    }));
                }
                Err(err) => {
                    let _ = tx.unbounded_send(Err(err));
                }
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
        // Completions are text-only, no multimodal support
        let generated = generate(
            &self.model,
            &self.cfg,
            self.mtp_model.as_ref(),
            &req.prompt,
            max_tokens,
            req.temperature,
            None,
            &[],
        )?;
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
