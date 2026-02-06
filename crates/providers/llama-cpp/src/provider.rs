use crate::backend::{install_abort_callback, llama_backend};
use crate::config::{DEFAULT_MAX_TOKENS, LlamaCppConfig, LlamaCppLogMode};
use crate::context::estimate_context_memory;
use crate::generation::{
    build_prompt, build_prompt_candidates, build_prompt_with, build_raw_prompt, generate,
    generate_streaming,
};
use crate::memory::MemoryEstimate;
use crate::response::LlamaCppChatResponse;
use crate::tools::{
    apply_template_with_tools, generate_streaming_with_tools, generate_with_tools,
    parse_tool_response,
};
use async_trait::async_trait;
use futures::Stream;
use futures::channel::mpsc;
use hf_hub::api::sync::ApiBuilder as SyncApiBuilder;
use hf_hub::api::tokio::ApiBuilder as AsyncApiBuilder;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::{LogOptions, send_logs_to_tracing};
use querymt::LLMProvider;
use querymt::chat::{ChatMessage, ChatProvider, ChatResponse, FinishReason, Tool};
use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
use querymt::embedding::EmbeddingProvider;
use querymt::error::LLMError;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

/// The main llama.cpp provider.
pub(crate) struct LlamaCppProvider {
    pub(crate) model: Arc<LlamaModel>,
    pub(crate) cfg: LlamaCppConfig,
}

impl LlamaCppProvider {
    /// Resolve a model path, potentially downloading from HuggingFace Hub.
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

    pub(crate) fn new(cfg: LlamaCppConfig) -> Result<Self, LLMError> {
        // Install the ggml abort callback before any llama.cpp operations.
        // This ensures that if Metal/CUDA triggers a fatal error, the user sees
        // a meaningful error message instead of just a raw stack trace.
        install_abort_callback();

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

        let provider = Self {
            model: Arc::new(model),
            cfg,
        };

        // Advisory memory warning at startup — never fails, just informs.
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

        Ok(provider)
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
                let template_result =
                    apply_template_with_tools(&self.model, &self.cfg, messages, tools)?;
                let generated = generate_with_tools(
                    &self.model,
                    &self.cfg,
                    &template_result,
                    max_tokens,
                    None,
                )?;
                let (content, thinking, tool_calls, finish_reason) =
                    parse_tool_response(&template_result, &generated.text)?;

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
        let (prompt, used_chat_template) = build_prompt(&self.model, &self.cfg, messages)?;
        let mut generated = generate(&self.model, &self.cfg, &prompt, max_tokens, None)?;
        if generated.text.trim().is_empty() {
            if used_chat_template && self.cfg.use_chat_template.is_none() {
                let (fallback_prompt, _) =
                    build_prompt_with(&self.model, &self.cfg, messages, false)?;
                generated = generate(&self.model, &self.cfg, &fallback_prompt, max_tokens, None)?;
            }
        }
        if generated.text.trim().is_empty() {
            let raw_prompt = build_raw_prompt(&self.cfg, messages)?;
            generated = generate(&self.model, &self.cfg, &raw_prompt, max_tokens, None)?;
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
                let template_result =
                    apply_template_with_tools(&self.model, &self.cfg, messages, tools)?;
                let cfg = self.cfg.clone();
                let model = Arc::clone(&self.model);

                thread::spawn(move || {
                    match generate_streaming_with_tools(
                        &model,
                        &cfg,
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
        let prompts = build_prompt_candidates(&self.model, &self.cfg, messages)?;
        let cfg = self.cfg.clone();
        let model = Arc::clone(&self.model);

        thread::spawn(move || {
            let mut final_usage = None;
            for (idx, prompt) in prompts.iter().enumerate() {
                match generate_streaming(&model, &cfg, prompt, max_tokens, None, &tx) {
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
        let generated = generate(
            &self.model,
            &self.cfg,
            &req.prompt,
            max_tokens,
            req.temperature,
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
