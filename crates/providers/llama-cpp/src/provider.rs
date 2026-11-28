use crate::backend::{install_abort_callback, llama_backend};
use crate::config::{DEFAULT_MAX_TOKENS, LlamaCppConfig, LlamaCppLogMode, SpeculativeConfig};
use crate::context::{ContextSettings, estimate_context_memory};
use crate::generation::{
    build_prompt, build_prompt_with, build_raw_prompt, generate, generate_streaming_with_thinking,
};
use crate::lora::{LoraSet, Models, locate_lora_adapters};
use crate::memory::MemoryEstimate;
use crate::multimodal::{MediaInputs, MultimodalContext};
use crate::placement::{ResolvedPlacement, available_devices};
use crate::response::LlamaCppChatResponse;
use crate::tools::{
    apply_template_for_thinking, apply_template_with_tools, generate_streaming_with_tools,
    generate_with_tools, parse_tool_response,
};
use async_trait::async_trait;
use futures::channel::mpsc;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::{LogOptions, send_logs_to_tracing};
use querymt::LLMProvider;
use querymt::chat::{ChatCall, ChatCapabilities, ChatOutput, ChatProvider, ChatStream};
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
    pub model_path: PathBuf,
    /// How the target's GPU offload is chosen (affects Metal/CUDA offloading).
    pub offload: Offload,
    /// Device selection, tensor split, split mode and main GPU. The sidecar's
    /// placement is derived from this, so it needs no separate key field.
    pub placement: ResolvedPlacement,
    /// Whether bundled MTP (NextN) tensors are loaded into the target model.
    pub load_mtp: bool,
    /// Resolved MTP sidecar path, when configured.
    pub mtp_model_path: Option<PathBuf>,
    /// Sidecar GPU layers.
    pub mtp_n_gpu_layers: Option<u32>,
    /// Resolved adapter paths in config order. Scales are deliberately not
    /// part of the key: they are applied per generation.
    pub lora_paths: Box<[PathBuf]>,
}

impl ModelCacheKey {
    /// Whether the target is fitted to free device memory at load time.
    fn is_fitted(&self) -> bool {
        matches!(self.offload, Offload::Fit(_))
    }

    /// Load params for the target model, and the context size a fit chose
    /// when `n_ctx` is not configured.
    fn target_params(
        &self,
    ) -> Result<(std::pin::Pin<Box<LlamaModelParams>>, Option<u32>), LLMError> {
        match &self.offload {
            Offload::Fixed(n_gpu_layers) => Ok((
                Box::pin(target_model_params(
                    &self.placement,
                    *n_gpu_layers,
                    self.load_mtp,
                )?),
                None,
            )),
            Offload::Fit(fit) => {
                fit_target_params(&self.model_path, fit, &self.placement, self.load_mtp)
            }
        }
    }
}

/// A cached model + multimodal context, shared across provider instances.
pub(crate) struct CachedModel {
    pub key: ModelCacheKey,
    pub model: Arc<LlamaModel>,
    pub multimodal: Option<Arc<MultimodalContext>>,
    pub mtp_model: Option<Arc<LlamaModel>>,
    pub loras: Arc<LoraSet>,
    /// Context size chosen by the fit when `n_ctx` was not configured.
    pub fitted_n_ctx: Option<u32>,
}

/// Whether MTP speculative decoding should use tensors bundled in the target GGUF.
///
/// llama-cpp-2 keeps `load_mtp` off by default, so the provider must opt in
/// when the target model is loaded. Only meaningful for MTP without a
/// sidecar model.
fn bundled_mtp_requested(cfg: &LlamaCppConfig) -> bool {
    cfg.speculative
        .as_ref()
        .is_some_and(SpeculativeConfig::uses_bundled_mtp)
}

/// Model-load params for the target model: GPU offload, device placement, and
/// opt-in loading of bundled MTP tensors.
fn target_model_params(
    placement: &ResolvedPlacement,
    n_gpu_layers: Option<u32>,
    load_mtp: bool,
) -> Result<LlamaModelParams, LLMError> {
    let mut params = LlamaModelParams::default();
    if let Some(n) = n_gpu_layers {
        params = params.with_n_gpu_layers(n);
    }
    if load_mtp {
        params = params.with_load_mtp(true);
    }
    placement.apply(params)
}

/// Model-load params for an MTP sidecar: always a single device (see
/// [`ResolvedPlacement::for_sidecar`]).
fn sidecar_model_params(
    placement: &ResolvedPlacement,
    n_gpu_layers: Option<u32>,
) -> Result<LlamaModelParams, LLMError> {
    let mut params = LlamaModelParams::default();
    if let Some(n) = n_gpu_layers {
        params = params.with_n_gpu_layers(n);
    }
    placement.for_sidecar().apply(params)
}

/// Smallest context the fit may pick when `fit_min_ctx` is not configured.
const DEFAULT_FIT_MIN_CTX: u32 = 4096;
/// Memory left free on each device when `fit_margin_mib` is not configured.
const DEFAULT_FIT_MARGIN_MIB: u32 = 1024;

/// How the target model's GPU offload is chosen.
///
/// A fit and a configured `n_gpu_layers` exclude each other: llama.cpp only
/// changes parameters that are still unset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Offload {
    /// `n_gpu_layers` as configured (`None` leaves it to llama.cpp).
    Fixed(Option<u32>),
    /// Fitted to free device memory at load time (`fit = true`).
    Fit(FitSettings),
}

/// Settings of a fit to free device memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FitSettings {
    n_ctx_min: u32,
    margin_bytes: usize,
    /// What the fit measures against.
    context: ContextSettings,
}

/// The offload to use for `cfg`.
///
/// `fit = true` has no effect when something it would choose is already
/// configured, so that falls back to [`Offload::Fixed`].
fn offload(cfg: &LlamaCppConfig, placement: &ResolvedPlacement) -> Offload {
    match fit_settings(cfg, placement) {
        Some(fit) => Offload::Fit(fit),
        None => Offload::Fixed(cfg.n_gpu_layers),
    }
}

/// The fit to run for `cfg`, or `None` when it is off or cannot apply.
///
/// llama.cpp only changes parameters that still hold their defaults, so a
/// configured `n_gpu_layers` or placement leaves nothing to fit, and the MTP
/// sidecar and bundled MTP tensors are not measured.
fn fit_settings(cfg: &LlamaCppConfig, placement: &ResolvedPlacement) -> Option<FitSettings> {
    if cfg.fit != Some(true) {
        return None;
    }
    let skipped = if cfg.n_gpu_layers.is_some() {
        Some("n_gpu_layers is set")
    } else if !placement.is_default() {
        Some("device placement is configured")
    } else if cfg.speculative.is_some() {
        Some("speculative decoding is configured")
    } else {
        None
    };
    if let Some(why) = skipped {
        log::info!("fit = true has no effect: {why}; fitting only changes unset parameters");
        return None;
    }
    Some(FitSettings {
        n_ctx_min: cfg.fit_min_ctx.unwrap_or(DEFAULT_FIT_MIN_CTX),
        margin_bytes: (cfg.fit_margin_mib.unwrap_or(DEFAULT_FIT_MARGIN_MIB) as usize) << 20,
        context: ContextSettings::from_config(cfg),
    })
}

/// Model-load params for the target, fitted to free device memory.
///
/// Also returns the context size the fit chose, when `n_ctx` is not
/// configured. When no allocation fits, the unfitted params are used and the
/// load is left to succeed or fail on its own, as `llama-cli` does.
#[cfg(feature = "common")]
fn fit_target_params(
    model_path: &Path,
    settings: &FitSettings,
    placement: &ResolvedPlacement,
    load_mtp: bool,
) -> Result<(std::pin::Pin<Box<LlamaModelParams>>, Option<u32>), LLMError> {
    use llama_cpp_2::model::params::FitError;

    let unfitted = || target_model_params(placement, None, load_mtp).map(Box::pin);
    let Some(path) = model_path
        .to_str()
        .and_then(|p| std::ffi::CString::new(p).ok())
    else {
        log::warn!("fit skipped: model path is not valid UTF-8 without NUL bytes");
        return Ok((unfitted()?, None));
    };
    let mut params = unfitted()?;
    let mut cparams = settings.context.fit_params()?;
    let mut margins = vec![settings.margin_bytes; llama_cpp_2::max_devices()];
    // 4 is GGML_LOG_LEVEL_ERROR: fit output below it goes to llama.cpp's debug log.
    let fitted =
        params
            .as_mut()
            .fit_params(&path, &mut cparams, &mut margins, settings.n_ctx_min, 4);
    match fitted {
        Ok(result) => {
            log::info!(
                "Fitted model to device memory: n_gpu_layers={}, n_ctx={}, cpu expert overrides={}",
                params.n_gpu_layers(),
                result.n_ctx,
                params.tensor_buft_override_patterns().len(),
            );
            Ok((
                params,
                settings.context.n_ctx.is_none().then_some(result.n_ctx),
            ))
        }
        Err(FitError::Failure) => {
            log::warn!(
                "No allocation fits the free device memory (fit_min_ctx={}); \
                 loading with the default parameters",
                settings.n_ctx_min
            );
            Ok((unfitted()?, None))
        }
        Err(FitError::Error) => Err(LLMError::ProviderError(format!(
            "Failed to fit {} to device memory",
            model_path.display()
        ))),
    }
}

#[cfg(not(feature = "common"))]
fn fit_target_params(
    _model_path: &Path,
    _settings: &FitSettings,
    placement: &ResolvedPlacement,
    load_mtp: bool,
) -> Result<(std::pin::Pin<Box<LlamaModelParams>>, Option<u32>), LLMError> {
    log::warn!("fit = true needs the `common` feature and is ignored");
    Ok((
        Box::pin(target_model_params(placement, None, load_mtp)?),
        None,
    ))
}

/// `cfg` with the context size a fit chose, unless one is configured.
fn with_fitted_n_ctx(mut cfg: LlamaCppConfig, fitted_n_ctx: Option<u32>) -> LlamaCppConfig {
    if cfg.n_ctx.is_none() {
        cfg.n_ctx = fitted_n_ctx;
    }
    cfg
}

/// The cache key for a model load. Adapter scales are not part of it.
fn cache_key(
    cfg: &LlamaCppConfig,
    model_path: &Path,
    mtp_model_path: Option<&Path>,
    lora_paths: &[PathBuf],
    placement: ResolvedPlacement,
) -> ModelCacheKey {
    ModelCacheKey {
        model_path: model_path.to_path_buf(),
        offload: offload(cfg, &placement),
        placement,
        load_mtp: bundled_mtp_requested(cfg),
        mtp_model_path: mtp_model_path.map(Path::to_path_buf),
        mtp_n_gpu_layers: cfg
            .speculative
            .as_ref()
            .and_then(|m| m.n_gpu_layers)
            .or(cfg.n_gpu_layers),
        lora_paths: lora_paths.into(),
    }
}

/// Resolve and validate the placement settings of `cfg` against the devices
/// llama.cpp currently sees. Requires the backend to be initialized.
fn resolve_placement(cfg: &LlamaCppConfig) -> Result<ResolvedPlacement, LLMError> {
    let placement = ResolvedPlacement::resolve(cfg, &available_devices())?;
    if !placement.is_default() {
        log::info!("Model placement: {}", placement.describe());
    }
    Ok(placement)
}

/// Models produced by [`load_models`].
struct LoadedModels {
    model: Arc<LlamaModel>,
    mtp_model: Option<Arc<LlamaModel>>,
    loras: Arc<LoraSet>,
    /// Context size the fit chose when `n_ctx` was not configured.
    fitted_n_ctx: Option<u32>,
}

/// Load the target model, its LoRA adapters and, when configured, the MTP sidecar,
/// as described by `key`.
///
/// The single place where placement is applied to model loading. Adapters go
/// onto the target model only, never onto the sidecar.
fn load_models(
    backend: &llama_cpp_2::llama_backend::LlamaBackend,
    key: &ModelCacheKey,
) -> Result<LoadedModels, LLMError> {
    let (params, fitted_n_ctx) = key.target_params()?;
    let target = LlamaModel::load_from_file(backend, &key.model_path, &params)
        .map_err(|e| LLMError::ProviderError(e.to_string()))?;
    let (model, loras) = LoraSet::load(target, &key.lora_paths)?;
    let loras = Arc::new(loras);
    let mtp_model = match key.mtp_model_path.as_deref() {
        Some(path) => Some(Arc::new(
            LlamaModel::load_from_file(
                backend,
                path,
                &sidecar_model_params(&key.placement, key.mtp_n_gpu_layers)?,
            )
            .map_err(|e| LLMError::ProviderError(format!("Failed to load MTP sidecar: {e}")))?,
        )),
        None => None,
    };
    Ok(LoadedModels {
        model,
        mtp_model,
        loras,
        fitted_n_ctx,
    })
}

/// The main llama.cpp provider.
pub(crate) struct LlamaCppProvider {
    pub(crate) model: Arc<LlamaModel>,
    pub(crate) cfg: LlamaCppConfig,
    pub(crate) multimodal: Option<Arc<MultimodalContext>>,
    pub(crate) mtp_model: Option<Arc<LlamaModel>>,
    pub(crate) loras: Arc<LoraSet>,
}

impl LlamaCppProvider {
    /// Borrowed view of the target model, optional draft model and adapters.
    pub(crate) fn models(&self) -> Models<'_> {
        Models {
            target: &self.model,
            draft: self.mtp_model.as_ref(),
            loras: &self.loras,
        }
    }

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

    /// Parse a model reference without touching the network or the disk.
    fn check_model_ref(raw: &str) -> Result<(), LLMError> {
        match parse_model_ref(raw).map_err(Self::map_model_ref_error)? {
            ModelRef::HfRepo(repo) => Err(LLMError::InvalidRequest(format!(
                "llama_cpp model must include a selector for Hugging Face repos: {repo}:<selector>"
            ))),
            ModelRef::LocalPath(_) | ModelRef::Hf(_) => Ok(()),
        }
    }

    /// Validate every adapter and model reference first (no network), then
    /// download the adapters. A bad `lora` or `model` value fails before any
    /// download starts, and adapters (small) are fetched before the base model.
    fn resolve_lora_paths(cfg: &LlamaCppConfig) -> Result<Box<[PathBuf]>, LLMError> {
        let located = locate_lora_adapters(&cfg.lora)?;
        Self::check_model_ref(&cfg.model)?;
        if let Some(sidecar) = cfg.speculative.as_ref().and_then(|m| m.model.as_deref()) {
            Self::check_model_ref(sidecar)?;
        }
        Ok(located.download()?)
    }

    fn map_model_ref_error(err: ModelRefError) -> LLMError {
        match err {
            ModelRefError::Invalid(msg) => LLMError::InvalidRequest(msg),
            ModelRefError::Download(msg) => LLMError::ProviderError(msg),
        }
    }

    pub(crate) fn new(cfg: LlamaCppConfig) -> Result<Self, LLMError> {
        // Validate adapters before anything is resolved, downloaded or loaded.
        let lora_paths = Self::resolve_lora_paths(&cfg)?;
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

        let sidecar_path = cfg
            .speculative
            .as_ref()
            .and_then(|m| m.model.as_deref())
            .map(Self::resolve_model_path)
            .transpose()?;
        let key = cache_key(
            &cfg,
            &model_path,
            sidecar_path.as_deref(),
            &lora_paths,
            resolve_placement(&cfg)?,
        );
        let LoadedModels {
            model,
            mtp_model,
            loras,
            fitted_n_ctx,
        } = load_models(&backend, &key)?;
        let cfg = with_fitted_n_ctx(cfg, fitted_n_ctx);

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
            loras,
        };
        Self::log_memory_advisory(&provider, key.is_fitted());
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
        // Validate adapters before anything is resolved, downloaded or loaded.
        let lora_paths = Self::resolve_lora_paths(&cfg)?;
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
        let key = cache_key(
            &cfg,
            &model_path,
            mtp_model_path.as_deref(),
            &lora_paths,
            resolve_placement(&cfg)?,
        );

        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.as_ref()
            && cached.key == key
        {
            return Ok(Self {
                model: Arc::clone(&cached.model),
                cfg: with_fitted_n_ctx(cfg, cached.fitted_n_ctx),
                multimodal: cached.multimodal.as_ref().map(Arc::clone),
                mtp_model: cached.mtp_model.as_ref().map(Arc::clone),
                loras: Arc::clone(&cached.loras),
            });
        }
        drop(guard);

        let model_path = key.model_path.as_path();
        if !model_path.exists() {
            return Err(LLMError::InvalidRequest(format!(
                "Model path does not exist: {}",
                model_path.display()
            )));
        }
        let LoadedModels {
            model,
            mtp_model,
            loras,
            fitted_n_ctx,
        } = load_models(&backend, &key)?;
        let cfg = with_fitted_n_ctx(cfg, fitted_n_ctx);

        let fitted = key.is_fitted();
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
            loras: Arc::clone(&loras),
            fitted_n_ctx,
        });

        let provider = Self {
            model,
            cfg,
            multimodal,
            mtp_model,
            loras,
        };
        Self::log_memory_advisory(&provider, fitted);
        Ok(provider)
    }

    /// Log advisory memory warnings at startup.
    ///
    /// `fitted` means llama.cpp already fitted the model to free memory. The
    /// estimate here assumes a full-size KV cache on every layer, which is
    /// pessimistic for sliding-window models, so it only informs then.
    fn log_memory_advisory(provider: &Self, fitted: bool) {
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
            if !fitted && est.gpu_memory_bytes > 0 && est.total_bytes > est.gpu_memory_bytes {
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
    fn capabilities(&self) -> ChatCapabilities {
        ChatCapabilities::NONE
            .with_streaming(true)
            .with_options(querymt::chat::GenerationOptionSupport::ALL)
    }

    async fn chat(&self, call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
        querymt::chat::reject_tool_changes("llama-cpp", call.messages)?;
        let (messages, tools) = (call.messages, call.tools);
        let cfg = self.cfg.with_overrides(call.options);
        let max_tokens = cfg.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

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
        let bitmaps = if let Some(mm_ctx) = self.multimodal.as_ref().filter(|_| !media.is_empty()) {
            let ctx = mm_ctx.lock()?;
            media
                .iter()
                .map(|m| m.to_bitmap(&ctx))
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
        let media_inputs = MediaInputs {
            ctx: active_multimodal,
            bitmaps: &bitmaps,
        };

        // If tools are provided and not empty, use tool-aware generation
        if let Some(tools) = tools
            && !tools.is_empty()
        {
            let template_result =
                apply_template_with_tools(&self.model, &cfg, messages, tools, media_marker)?;
            let generated = generate_with_tools(
                self.models(),
                &cfg,
                &template_result,
                max_tokens,
                None,
                media_inputs,
            )?;
            let (content, thinking, tool_calls, _) =
                parse_tool_response(&template_result, &generated.text)?;
            let finish_reason = generated
                .termination
                .finish_reason(tool_calls.as_ref().is_some_and(|calls| !calls.is_empty()));

            return Ok(LlamaCppChatResponse {
                text: content,
                thinking,
                tool_calls,
                finish_reason,
                usage: generated.usage,
            }
            .into());
        }

        // Structured output: use OAI-compat template so the schema is converted
        // to a GBNF grammar that constrains sampling to valid JSON.
        if cfg.json_schema.is_some() {
            let template_result =
                apply_template_for_thinking(&self.model, &cfg, messages, media_marker)?;
            let generated = generate_with_tools(
                self.models(),
                &cfg,
                &template_result,
                max_tokens,
                None,
                media_inputs,
            )?;
            let (content, thinking, _tool_calls, _) =
                parse_tool_response(&template_result, &generated.text)?;
            let finish_reason = generated.termination.finish_reason(false);
            return Ok(LlamaCppChatResponse {
                text: content,
                thinking,
                tool_calls: None,
                finish_reason,
                usage: generated.usage,
            }
            .into());
        }

        // Standard generation (with or without images)
        let (prompt, used_chat_template) = build_prompt(&self.model, &cfg, messages, media_marker)?;

        // Call unified generate() with optional multimodal params
        let mut generated = generate(self.models(), &cfg, &prompt, max_tokens, None, media_inputs)?;
        // Retry alternate prompt formats only when the model ended immediately.
        if generated.text.trim().is_empty()
            && generated.termination == crate::response::GenerationTermination::Eog
            && used_chat_template
            && cfg.use_chat_template.is_none()
        {
            let (fallback_prompt, _) =
                build_prompt_with(&self.model, &cfg, messages, false, media_marker)?;
            generated = generate(
                self.models(),
                &cfg,
                &fallback_prompt,
                max_tokens,
                None,
                media_inputs,
            )?;
        }
        if generated.text.trim().is_empty()
            && generated.termination == crate::response::GenerationTermination::Eog
        {
            let raw_prompt = build_raw_prompt(&cfg, messages)?;
            generated = generate(
                self.models(),
                &cfg,
                &raw_prompt,
                max_tokens,
                None,
                media_inputs,
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
        Ok(LlamaCppChatResponse {
            text: clean_text,
            thinking,
            tool_calls: None,
            finish_reason: generated.termination.finish_reason(false),
            usage: generated.usage,
        }
        .into())
    }

    async fn chat_stream(&self, call: &ChatCall<'_>) -> Result<ChatStream, LLMError> {
        querymt::chat::reject_tool_changes("llama-cpp", call.messages)?;
        let (messages, tools) = (call.messages, call.tools);
        let cfg = self.cfg.with_overrides(call.options);
        let max_tokens = cfg.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
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
        let bitmaps = if let Some(mm_ctx) = self.multimodal.as_ref().filter(|_| !media.is_empty()) {
            let ctx = mm_ctx.lock()?;
            media
                .iter()
                .map(|m| m.to_bitmap(&ctx))
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
        if let Some(tools) = tools
            && !tools.is_empty()
        {
            let template_result =
                apply_template_with_tools(&self.model, &cfg, messages, tools, media_marker)?;
            let cfg = cfg;
            let model = Arc::clone(&self.model);
            let mtp_model = self.mtp_model.clone();
            let loras = Arc::clone(&self.loras);
            let multimodal = if bitmaps.is_empty() {
                None
            } else {
                self.multimodal.clone()
            };

            thread::spawn(move || {
                match generate_streaming_with_tools(
                    Models {
                        target: &model,
                        draft: mtp_model.as_ref(),
                        loras: &loras,
                    },
                    &cfg,
                    &template_result,
                    max_tokens,
                    None,
                    &tx,
                    MediaInputs {
                        ctx: multimodal.as_deref(),
                        bitmaps: &bitmaps,
                    },
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

        // No-tool streaming uses the same Rust-side template path as tool
        // streaming so thinking markers are routed to StreamChunk::Thinking.
        // Template failures are surfaced instead of silently degrading to raw
        // streaming, which would leak <think> tags to the UI.
        let thinking_template =
            apply_template_for_thinking(&self.model, &cfg, messages, media_marker)?;
        let cfg = cfg;
        let model = Arc::clone(&self.model);
        let mtp_model = self.mtp_model.clone();
        let loras = Arc::clone(&self.loras);
        let multimodal = if bitmaps.is_empty() {
            None
        } else {
            self.multimodal.clone()
        };

        thread::spawn(move || {
            match generate_streaming_with_thinking(
                Models {
                    target: &model,
                    draft: mtp_model.as_ref(),
                    loras: &loras,
                },
                &cfg,
                &thinking_template,
                max_tokens,
                None,
                &tx,
                MediaInputs {
                    ctx: multimodal.as_deref(),
                    bitmaps: &bitmaps,
                },
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
            self.models(),
            &self.cfg,
            &req.prompt,
            max_tokens,
            req.temperature,
            MediaInputs::default(),
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

#[cfg(test)]
mod tests {
    use super::{
        ModelCacheKey, Offload, bundled_mtp_requested, cache_key, fit_settings, offload,
        sidecar_model_params, target_model_params, with_fitted_n_ctx,
    };
    use crate::config::{
        LlamaCppConfig, LoraAdapterConfig, LoraScale, SpeculativeConfig, SpeculativeType,
    };
    use crate::placement::ResolvedPlacement;
    use std::path::{Path, PathBuf};

    fn base_config() -> LlamaCppConfig {
        LlamaCppConfig {
            model: "/path/to/model.gguf".to_string(),
            ..Default::default()
        }
    }

    fn speculative(model: Option<&str>) -> Option<SpeculativeConfig> {
        Some(SpeculativeConfig {
            kind: SpeculativeType::Mtp,
            model: model.map(str::to_string),
            n_max: None,
            n_min: None,
            p_min: None,
            n_gpu_layers: None,
            main_gpu: None,
        })
    }

    #[test]
    fn bundled_mtp_only_without_sidecar() {
        let mut cfg = base_config();
        assert!(!bundled_mtp_requested(&cfg), "no speculative config");

        cfg.speculative = speculative(None);
        assert!(bundled_mtp_requested(&cfg), "bundled MTP without sidecar");

        cfg.speculative = speculative(Some("draft.gguf"));
        assert!(!bundled_mtp_requested(&cfg), "sidecar overrides bundled");
    }

    #[test]
    fn target_params_enable_mtp_only_when_requested() {
        let p = ResolvedPlacement::default();
        assert!(
            !target_model_params(&p, None, false).unwrap().load_mtp(),
            "load_mtp stays off by default"
        );
        assert!(
            target_model_params(&p, Some(33), true).unwrap().load_mtp(),
            "load_mtp enabled when requested"
        );
        assert_eq!(
            target_model_params(&p, Some(33), false)
                .unwrap()
                .n_gpu_layers(),
            33,
            "gpu layers preserved"
        );
    }

    fn placement_for(extra: &str) -> ResolvedPlacement {
        use crate::placement::DeviceInfo;
        let cfg: LlamaCppConfig =
            serde_json::from_str(&format!(r#"{{"model":"m.gguf",{extra}}}"#)).unwrap();
        let devs: Vec<DeviceInfo> = (0..2)
            .map(|index| DeviceInfo {
                index,
                name: format!("CUDA{index}"),
            })
            .collect();
        ResolvedPlacement::resolve(&cfg, &devs).unwrap()
    }

    #[test]
    fn target_params_carry_tensor_split() {
        let p = placement_for(r#""tensor_split":[3,1]"#);
        let params = target_model_params(&p, None, false).unwrap();
        assert_eq!(&params.tensor_split()[..2], &[3.0, 1.0]);
    }

    #[test]
    fn sidecar_params_never_split() {
        let p = placement_for(r#""tensor_split":[3,1],"split_mode":"layer""#);
        let params = sidecar_model_params(&p, Some(7)).unwrap();
        assert!(params.tensor_split().is_empty(), "sidecar is not split");
        assert_eq!(params.n_gpu_layers(), 7);
    }

    #[test]
    fn cache_key_distinguishes_placement() {
        let base = ModelCacheKey {
            model_path: PathBuf::from("/path/to/model.gguf"),
            offload: Offload::Fixed(None),
            placement: placement_for(r#""tensor_split":[3,1]"#),
            load_mtp: false,
            mtp_model_path: None,
            mtp_n_gpu_layers: None,
            lora_paths: Box::default(),
        };
        let mut other = base.clone();
        other.placement = placement_for(r#""tensor_split":[1,1]"#);
        assert_ne!(base, other, "tensor_split must invalidate the cache key");
    }

    fn key_for(cfg: &LlamaCppConfig, lora_paths: &[&str]) -> ModelCacheKey {
        let paths: Vec<PathBuf> = lora_paths.iter().map(PathBuf::from).collect();
        cache_key(
            cfg,
            Path::new("/path/to/model.gguf"),
            None,
            &paths,
            ResolvedPlacement::default(),
        )
    }

    #[test]
    fn changed_adapter_list_reloads_the_model() {
        let cfg = base_config();
        let a = key_for(&cfg, &["/models/a.gguf"]);
        let b = key_for(&cfg, &["/models/b.gguf"]);
        let none = key_for(&cfg, &[]);
        assert_ne!(a, b);
        assert_ne!(a, none);
        assert_ne!(b, none);
    }

    #[test]
    fn changed_scale_reuses_the_loaded_model() {
        let scaled = |scale: f32| {
            let mut cfg = base_config();
            cfg.lora = vec![
                LoraAdapterConfig::new("/models/a.gguf").with_scale(LoraScale::new(scale).unwrap()),
            ];
            key_for(&cfg, &["/models/a.gguf"])
        };
        assert_eq!(scaled(1.0), scaled(0.5));
    }

    #[test]
    fn more_than_one_adapter_rejected_before_model_load() {
        use crate::provider::LlamaCppProvider;
        use querymt::error::LLMError;

        let mut cfg = base_config();
        cfg.model = "/nonexistent/base-model.gguf".to_owned();
        cfg.lora = vec![
            LoraAdapterConfig::new("/nonexistent/a.gguf"),
            LoraAdapterConfig::new("/nonexistent/b.gguf"),
        ];

        let Some(LLMError::InvalidRequest(message)) = LlamaCppProvider::new(cfg).err() else {
            panic!("expected an InvalidRequest error");
        };
        assert!(message.contains("only one"), "{message}");
        assert!(!message.contains("Model path does not exist"), "{message}");
    }

    #[test]
    fn invalid_model_ref_rejected_before_adapter_download() {
        use crate::provider::LlamaCppProvider;
        use querymt::error::LLMError;

        // The adapter is a Hugging Face ref (it would need the network), the
        // model is a repo without selector. The model error must win without
        // any download being attempted.
        let mut cfg = base_config();
        cfg.model = "owner/repo".to_owned();
        cfg.lora = vec![LoraAdapterConfig::new("owner/repo:adapter.gguf")];

        let Some(LLMError::InvalidRequest(message)) = LlamaCppProvider::new(cfg).err() else {
            panic!("expected an InvalidRequest error");
        };
        assert!(message.contains("selector"), "{message}");
    }

    #[test]
    fn cache_key_distinguishes_bundled_mtp() {
        let base = ModelCacheKey {
            model_path: PathBuf::from("/path/to/model.gguf"),
            offload: Offload::Fixed(None),
            placement: ResolvedPlacement::default(),
            load_mtp: false,
            mtp_model_path: None,
            mtp_n_gpu_layers: None,
            lora_paths: Box::default(),
        };
        let mut with_mtp = base.clone();
        with_mtp.load_mtp = true;
        assert_ne!(base, with_mtp, "load_mtp must invalidate the cache key");
    }

    fn fit_config() -> LlamaCppConfig {
        let mut cfg = base_config();
        cfg.fit = Some(true);
        cfg
    }

    #[test]
    fn fit_is_off_unless_requested() {
        let placement = ResolvedPlacement::default();
        assert_eq!(fit_settings(&base_config(), &placement), None);
        let mut cfg = base_config();
        cfg.fit = Some(false);
        assert_eq!(fit_settings(&cfg, &placement), None);
    }

    #[test]
    fn fit_uses_llama_cli_defaults() {
        let settings = fit_settings(&fit_config(), &ResolvedPlacement::default()).unwrap();
        assert_eq!(settings.n_ctx_min, 4096);
        assert_eq!(settings.margin_bytes, 1024 << 20);

        let mut cfg = fit_config();
        cfg.fit_min_ctx = Some(8192);
        cfg.fit_margin_mib = Some(512);
        let settings = fit_settings(&cfg, &ResolvedPlacement::default()).unwrap();
        assert_eq!(settings.n_ctx_min, 8192);
        assert_eq!(settings.margin_bytes, 512 << 20);
    }

    #[test]
    fn fit_is_skipped_when_nothing_is_left_to_fit() {
        let default = ResolvedPlacement::default();

        let mut cfg = fit_config();
        cfg.n_gpu_layers = Some(99);
        assert_eq!(fit_settings(&cfg, &default), None, "n_gpu_layers is set");

        let mut cfg = fit_config();
        cfg.speculative = speculative(None);
        assert_eq!(fit_settings(&cfg, &default), None, "speculative decoding");

        let split = placement_for(r#""tensor_split":[3,1]"#);
        assert_eq!(fit_settings(&fit_config(), &split), None, "placement set");
    }

    #[test]
    fn cache_key_follows_what_the_fit_measured() {
        let fitted = |edit: fn(&mut LlamaCppConfig)| {
            let mut cfg = fit_config();
            edit(&mut cfg);
            key_for(&cfg, &[])
        };
        let base = fitted(|_| {});
        assert_eq!(base, fitted(|_| {}));
        assert_ne!(base, key_for(&base_config(), &[]), "fit on vs off");
        assert_ne!(base, fitted(|c| c.n_ctx = Some(120_000)));
        assert_ne!(base, fitted(|c| c.kv_cache_type_k = Some("q8_0".into())));
        assert_ne!(base, fitted(|c| c.swa_full = Some(true)));
        assert_ne!(base, fitted(|c| c.fit_min_ctx = Some(8192)));
        assert_ne!(base, fitted(|c| c.fit_margin_mib = Some(2048)));
    }

    #[test]
    fn fit_and_n_gpu_layers_are_exclusive_in_the_offload() {
        let default = ResolvedPlacement::default();
        assert_eq!(offload(&base_config(), &default), Offload::Fixed(None));

        let mut cfg = base_config();
        cfg.n_gpu_layers = Some(99);
        assert_eq!(offload(&cfg, &default), Offload::Fixed(Some(99)));

        assert!(matches!(offload(&fit_config(), &default), Offload::Fit(_)));

        // `fit = true` with `n_gpu_layers` set has nothing to fit: the
        // configured layer count wins.
        let mut cfg = fit_config();
        cfg.n_gpu_layers = Some(99);
        assert_eq!(offload(&cfg, &default), Offload::Fixed(Some(99)));
    }

    #[test]
    fn fixed_offload_loads_with_the_configured_layers_and_no_fitted_context() {
        let mut cfg = base_config();
        cfg.n_gpu_layers = Some(99);
        let key = key_for(&cfg, &[]);
        assert!(!key.is_fitted());
        let (params, fitted_n_ctx) = key.target_params().unwrap();
        assert_eq!(params.n_gpu_layers(), 99);
        assert_eq!(fitted_n_ctx, None);
    }

    #[test]
    fn sidecar_key_carries_its_path_and_gpu_layers() {
        let mut cfg = base_config();
        cfg.n_gpu_layers = Some(20);
        cfg.speculative = speculative(Some("/models/draft.gguf"));

        let key = cache_key(
            &cfg,
            Path::new("/path/to/model.gguf"),
            Some(Path::new("/models/draft.gguf")),
            &[],
            ResolvedPlacement::default(),
        );
        assert_eq!(key.model_path, Path::new("/path/to/model.gguf"));
        assert_eq!(
            key.mtp_model_path.as_deref(),
            Some(Path::new("/models/draft.gguf"))
        );
        assert!(!key.load_mtp, "a sidecar replaces the bundled tensors");
        // The sidecar follows the target's layer count unless it sets its own.
        assert_eq!(key.mtp_n_gpu_layers, Some(20));

        cfg.speculative.as_mut().unwrap().n_gpu_layers = Some(5);
        let key = cache_key(
            &cfg,
            Path::new("/path/to/model.gguf"),
            Some(Path::new("/models/draft.gguf")),
            &[],
            ResolvedPlacement::default(),
        );
        assert_eq!(key.mtp_n_gpu_layers, Some(5));
        assert_eq!(
            key.offload,
            Offload::Fixed(Some(20)),
            "target is unaffected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cache_key_keeps_non_utf8_paths_intact() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new(OsStr::from_bytes(b"/models/caf\xe9.gguf"));
        let key = cache_key(
            &base_config(),
            path,
            None,
            &[],
            ResolvedPlacement::default(),
        );
        assert_eq!(key.model_path, path, "a lossy conversion would change it");
        let other = Path::new(OsStr::from_bytes(b"/models/caf\xe8.gguf"));
        let other = cache_key(
            &base_config(),
            other,
            None,
            &[],
            ResolvedPlacement::default(),
        );
        assert_ne!(key, other, "distinct paths are distinct keys");
    }

    #[test]
    fn fitted_context_never_overrides_a_configured_one() {
        let mut cfg = base_config();
        assert_eq!(
            with_fitted_n_ctx(cfg.clone(), Some(32_768)).n_ctx,
            Some(32_768)
        );
        assert_eq!(with_fitted_n_ctx(cfg.clone(), None).n_ctx, None);
        cfg.n_ctx = Some(8192);
        assert_eq!(with_fitted_n_ctx(cfg, Some(32_768)).n_ctx, Some(8192));
    }
}
