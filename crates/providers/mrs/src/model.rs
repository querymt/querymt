use std::{
    env, fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use hf_hub::{Cache, Repo, RepoType, api::sync::ApiBuilder};
use mistralrs::core::{
    EmbeddingLoaderType, MultimodalLoaderType, NormalLoaderType, PagedCacheType, SpeechLoaderType,
};
use mistralrs::{
    AudioInput, DeviceMapSetting, EmbeddingModelBuilder, EmbeddingRequestBuilder, GgufModelBuilder,
    IsqType, MemoryGpuConfig, Model, ModelDType, MultimodalModelBuilder, PagedAttentionConfig,
    RequestBuilder, SpeechModelBuilder, TextMessageRole, TextModelBuilder, TokenSource, Topology,
    parse_isq_value, speech_utils,
};
use querymt::chat::Tool;
use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
use querymt::embedding::EmbeddingProvider;
use querymt::stt::{SttRequest, SttResponse};
use querymt::tts::{TtsRequest, TtsResponse};
use querymt::{LLMProvider, error::LLMError};
use querymt_provider_common::{ModelRef, parse_model_ref};
use serde::Deserialize;

use crate::config::{
    MistralRSConfig, MistralRSDeviceMap, MistralRSModelKind, MistralRSPagedCacheType,
};
use crate::messages::ensure_embedding_model;

/// Cache key for model loading — only params that affect the loaded `Model`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelCacheKey {
    pub model: String,
    pub model_kind: Option<MistralRSModelKind>,
    pub dtype: Option<String>,
    pub force_cpu: bool,
}

/// A cached model, shared across provider instances via `Arc`.
pub(crate) struct CachedModel {
    pub key: ModelCacheKey,
    pub model: std::sync::Arc<Model>,
}

pub struct MistralRS {
    pub config: MistralRSConfig,
    pub mrs_model: std::sync::Arc<Model>,
}

impl MistralRS {
    /// Build a provider, reusing a cached model if the cache key matches.
    ///
    /// Model loading is the expensive operation (downloads + GPU init).
    /// The cache stores the loaded `Arc<Model>`. Each call returns a cheap
    /// provider wrapper that shares the cached model but carries its own
    /// per-request config (tools, system prompt, etc.).
    pub(crate) fn new_with_cache(
        cfg: MistralRSConfig,
        cache: &std::sync::Mutex<Option<CachedModel>>,
    ) -> Result<Self, LLMError> {
        let key = ModelCacheKey {
            model: cfg.model.clone(),
            model_kind: cfg.model_kind,
            dtype: cfg.dtype.clone(),
            force_cpu: cfg.force_cpu.unwrap_or(false),
        };

        // Fast path: check cache under lock, return immediately on hit.
        {
            let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ref cached) = *guard {
                if cached.key == key {
                    log::debug!("MistralRS model cache hit: {}", key.model);
                    return Ok(Self {
                        config: cfg,
                        mrs_model: std::sync::Arc::clone(&cached.model),
                    });
                }
                log::info!(
                    "MistralRS model cache evict: {} -> {}",
                    cached.key.model,
                    key.model
                );
            }
        }
        // Lock released — expensive model load happens outside the lock.

        let provider = match tokio::runtime::Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| handle.block_on(MistralRS::new(cfg)))?,
            Err(_) => {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))?;
                runtime.block_on(MistralRS::new(cfg))?
            }
        };

        // Store in cache.
        let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(CachedModel {
            key,
            model: std::sync::Arc::clone(&provider.mrs_model),
        });

        Ok(provider)
    }

    pub async fn new(cfg: MistralRSConfig) -> Result<Self, LLMError> {
        let gguf_spec = gguf_spec_from_config(&cfg)?;
        let model_kind = match cfg.model_kind {
            Some(kind) => kind,
            None => {
                if gguf_spec.is_some() {
                    MistralRSModelKind::Text
                } else {
                    infer_model_kind(&cfg)?
                }
            }
        };
        let m = match gguf_spec {
            Some(spec) => match model_kind {
                MistralRSModelKind::Text => build_gguf_model(&cfg, spec).await?,
                _ => {
                    return Err(LLMError::InvalidRequest(
                        "gguf loading is only supported for text models".into(),
                    ));
                }
            },
            None => match model_kind {
                MistralRSModelKind::Text => build_text_model(&cfg).await?,
                MistralRSModelKind::Vision | MistralRSModelKind::Audio => {
                    build_vision_model(&cfg).await?
                }
                MistralRSModelKind::Embedding => build_embedding_model(&cfg).await?,
                MistralRSModelKind::Speech => build_speech_model(&cfg).await?,
            },
        };

        Ok(Self {
            config: cfg,
            mrs_model: std::sync::Arc::new(m),
        })
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for MistralRS {
    async fn embed(&self, input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        ensure_embedding_model(&self.mrs_model)?;
        let request = EmbeddingRequestBuilder::new().add_prompts(input);
        self.mrs_model
            .generate_embeddings(request)
            .await
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))
    }
}

#[async_trait::async_trait]
impl CompletionProvider for MistralRS {
    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        let _ = req;
        Err(LLMError::NotImplemented(
            "mistralrs provider does not support text completion".into(),
        ))
    }
}

#[async_trait::async_trait]
impl LLMProvider for MistralRS {
    fn tools(&self) -> Option<&[Tool]> {
        self.config.tools.as_deref()
    }

    async fn speech(&self, req: &TtsRequest) -> Result<TtsResponse, LLMError> {
        let kind = self.config.model_kind.unwrap_or_default();
        if !matches!(kind, MistralRSModelKind::Speech) {
            return Err(LLMError::NotImplemented(
                "TTS requires model_kind = \"speech\"".into(),
            ));
        }

        let (pcm, rate, channels) = self
            .mrs_model
            .generate_speech(&req.text)
            .await
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))?;

        let mut wav_buf = Vec::new();
        speech_utils::write_pcm_as_wav(&mut wav_buf, &pcm, rate as u32, channels as u16)
            .map_err(|e| LLMError::ProviderError(format!("WAV encoding failed: {e}")))?;

        Ok(TtsResponse {
            audio: wav_buf,
            mime_type: Some("audio/wav".into()),
        })
    }

    async fn transcribe(&self, req: &SttRequest) -> Result<SttResponse, LLMError> {
        let kind = self.config.model_kind.unwrap_or_default();
        if !matches!(kind, MistralRSModelKind::Audio | MistralRSModelKind::Vision) {
            return Err(LLMError::NotImplemented(
                "STT requires model_kind = \"audio\" or \"vision\" (multimodal model)".into(),
            ));
        }

        let audio = AudioInput::from_bytes(&req.audio)
            .map_err(|e| LLMError::InvalidRequest(format!("invalid audio data: {e}")))?;

        let request = RequestBuilder::new().add_audio_message(
            TextMessageRole::User,
            "Transcribe this audio.",
            vec![audio],
        );

        let response = self
            .mrs_model
            .send_chat_request(request)
            .await
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))?;

        let text = response
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();

        Ok(SttResponse { text })
    }
}

#[derive(Debug)]
struct ModelConfigArtifacts {
    contents: String,
    sentence_transformers_present: bool,
}

#[derive(Deserialize)]
struct ModelAutoConfig {
    #[serde(default)]
    architectures: Vec<String>,
}

fn token_source_override(cfg: &MistralRSConfig) -> Result<Option<TokenSource>, LLMError> {
    cfg.token_source
        .as_deref()
        .map(TokenSource::from_str)
        .transpose()
        .map_err(LLMError::InvalidRequest)
}

fn infer_model_kind(cfg: &MistralRSConfig) -> Result<MistralRSModelKind, LLMError> {
    let artifacts = load_model_config_artifacts(cfg).map_err(|e| {
        LLMError::InvalidRequest(format!(
            "unable to infer model kind from config: {e}; set model_kind explicitly"
        ))
    })?;
    let auto_cfg: ModelAutoConfig = serde_json::from_str(&artifacts.contents).map_err(|e| {
        LLMError::InvalidRequest(format!("unable to parse model config for detection: {e}"))
    })?;

    if artifacts.sentence_transformers_present {
        return Ok(MistralRSModelKind::Embedding);
    }

    if SpeechLoaderType::auto_detect_from_config(&artifacts.contents).is_some() {
        return Ok(MistralRSModelKind::Speech);
    }

    if let Some(name) = auto_cfg.architectures.first() {
        if MultimodalLoaderType::from_causal_lm_name(name).is_ok() {
            return Ok(MistralRSModelKind::Vision);
        }
        if EmbeddingLoaderType::from_causal_lm_name(name).is_ok() {
            return Ok(MistralRSModelKind::Embedding);
        }
    }

    Ok(MistralRSModelKind::Text)
}

fn load_model_config_artifacts(cfg: &MistralRSConfig) -> Result<ModelConfigArtifacts, LLMError> {
    let model_path = Path::new(&cfg.model);
    if model_path.exists() {
        return load_model_config_from_path(model_path);
    }

    let token_source = resolve_token_source(cfg)?;
    load_model_config_from_hf(&cfg.model, cfg.hf_revision.as_deref(), &token_source)
}

fn load_model_config_from_path(path: &Path) -> Result<ModelConfigArtifacts, LLMError> {
    let config_path = if path.is_dir() {
        path.join("config.json")
    } else {
        path.to_path_buf()
    };
    let contents = fs::read_to_string(&config_path).map_err(|e| {
        LLMError::InvalidRequest(format!(
            "unable to read model config at {}: {e}",
            config_path.display()
        ))
    })?;
    let sentence_transformers_present = config_path
        .parent()
        .map(|parent| parent.join("config_sentence_transformers.json").exists())
        .unwrap_or(false);
    Ok(ModelConfigArtifacts {
        contents,
        sentence_transformers_present,
    })
}

fn load_model_config_from_hf(
    model_id: &str,
    revision: Option<&str>,
    token_source: &TokenSource,
) -> Result<ModelConfigArtifacts, LLMError> {
    let token = token_from_source(token_source)?;
    let cache = Cache::from_env();
    let mut api = ApiBuilder::from_cache(cache)
        .with_progress(false)
        .with_token(token);
    if let Ok(cache_dir) = env::var("HF_HUB_CACHE") {
        api = api.with_cache_dir(cache_dir.into());
    }
    let api = api
        .build()
        .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))?;
    let revision = revision.unwrap_or("main");
    let repo = api.repo(Repo::with_revision(
        model_id.to_string(),
        RepoType::Model,
        revision.to_string(),
    ));
    let config_path = repo
        .get("config.json")
        .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))?;
    let contents = fs::read_to_string(&config_path)
        .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))?;
    let sentence_transformers_present = repo.get("config_sentence_transformers.json").is_ok();
    Ok(ModelConfigArtifacts {
        contents,
        sentence_transformers_present,
    })
}

fn resolve_token_source(cfg: &MistralRSConfig) -> Result<TokenSource, LLMError> {
    match cfg.token_source.as_ref() {
        Some(token_source) => TokenSource::from_str(token_source).map_err(LLMError::InvalidRequest),
        None => Ok(TokenSource::CacheToken),
    }
}

fn token_from_source(source: &TokenSource) -> Result<Option<String>, LLMError> {
    let token = match source {
        TokenSource::Literal(data) => Some(data.clone()),
        TokenSource::EnvVar(envvar) => env::var(envvar).ok(),
        TokenSource::Path(path) => fs::read_to_string(path).ok(),
        TokenSource::CacheToken => {
            let home = env::var("HOME").or_else(|_| env::var("USERPROFILE")).ok();
            home.and_then(|path| {
                fs::read_to_string(Path::new(&path).join(".cache/huggingface/token")).ok()
            })
        }
        TokenSource::None => None,
    };
    Ok(token.map(|s| s.trim().to_string()))
}

fn device_map_setting(cfg: &MistralRSConfig, kind: MistralRSModelKind) -> Option<DeviceMapSetting> {
    match cfg.device_map {
        Some(MistralRSDeviceMap::Single) => Some(DeviceMapSetting::dummy()),
        Some(MistralRSDeviceMap::Auto) => None,
        None => {
            if matches!(kind, MistralRSModelKind::Vision | MistralRSModelKind::Audio)
                && cfg!(feature = "metal")
                && !cfg.force_cpu.unwrap_or(false)
            {
                Some(DeviceMapSetting::dummy())
            } else {
                None
            }
        }
    }
}

fn isq_from_config(cfg: &MistralRSConfig) -> Result<Option<IsqType>, LLMError> {
    let Some(isq) = cfg.isq.as_deref() else {
        return Ok(None);
    };
    parse_isq_value(isq, None)
        .map(Some)
        .map_err(|e| LLMError::InvalidRequest(format!("invalid isq value: {e}")))
}

fn dtype_from_config(cfg: &MistralRSConfig) -> Result<Option<ModelDType>, LLMError> {
    let Some(dtype) = cfg.dtype.as_deref() else {
        return Ok(None);
    };
    ModelDType::from_str(dtype)
        .map(Some)
        .map_err(|e| LLMError::InvalidRequest(format!("invalid dtype value: {e}")))
}

fn topology_from_config(cfg: &MistralRSConfig) -> Result<Option<Topology>, LLMError> {
    let Some(topology) = cfg.topology.as_deref() else {
        return Ok(None);
    };
    let raw = if Path::new(topology).exists() {
        fs::read_to_string(topology).map_err(|e| {
            LLMError::InvalidRequest(format!("unable to read topology file {topology}: {e}"))
        })?
    } else {
        topology.to_string()
    };
    Topology::from_str(&raw)
        .map(Some)
        .map_err(|e| LLMError::InvalidRequest(format!("invalid topology: {e}")))
}

fn text_loader_type(cfg: &MistralRSConfig) -> Result<Option<NormalLoaderType>, LLMError> {
    cfg.loader_type
        .as_deref()
        .map(NormalLoaderType::from_str)
        .transpose()
        .map_err(|e| LLMError::InvalidRequest(format!("invalid loader_type value: {e}")))
}

fn vision_loader_type(cfg: &MistralRSConfig) -> Result<Option<MultimodalLoaderType>, LLMError> {
    cfg.loader_type
        .as_deref()
        .map(MultimodalLoaderType::from_str)
        .transpose()
        .map_err(|e| LLMError::InvalidRequest(format!("invalid loader_type value: {e}")))
}

fn embedding_loader_type(cfg: &MistralRSConfig) -> Result<Option<EmbeddingLoaderType>, LLMError> {
    cfg.loader_type
        .as_deref()
        .map(EmbeddingLoaderType::from_str)
        .transpose()
        .map_err(|e| LLMError::InvalidRequest(format!("invalid loader_type value: {e}")))
}

fn speech_loader_type(cfg: &MistralRSConfig) -> Result<SpeechLoaderType, LLMError> {
    let s = cfg.speech_loader_type.as_deref().ok_or_else(|| {
        LLMError::InvalidRequest(
            "speech_loader_type is required for speech models (e.g. \"dia\")".into(),
        )
    })?;
    SpeechLoaderType::from_str(s)
        .map_err(|e| LLMError::InvalidRequest(format!("invalid speech_loader_type: {e}")))
}

fn paged_cache_type_from_config(cache_type: MistralRSPagedCacheType) -> PagedCacheType {
    match cache_type {
        MistralRSPagedCacheType::Auto => PagedCacheType::Auto,
        MistralRSPagedCacheType::F8E4M3 => PagedCacheType::F8E4M3,
    }
}

fn paged_attn_config(cfg: &MistralRSConfig) -> Result<Option<PagedAttentionConfig>, LLMError> {
    let has_settings = cfg.paged_attn_block_size.is_some()
        || cfg.paged_attn_gpu_mem.is_some()
        || cfg.paged_attn_gpu_mem_usage.is_some()
        || cfg.paged_attn_context_len.is_some()
        || cfg.paged_attn_cache_type.is_some();
    let enabled = cfg.paged_attn.unwrap_or(has_settings);

    if !enabled {
        if has_settings {
            return Err(LLMError::InvalidRequest(
                "paged_attn is disabled but paged_attn_* settings were provided".into(),
            ));
        }
        return Ok(None);
    }

    let cache_type = cfg
        .paged_attn_cache_type
        .map(paged_cache_type_from_config)
        .unwrap_or(PagedCacheType::Auto);

    let mem_gpu = match (
        cfg.paged_attn_gpu_mem,
        cfg.paged_attn_gpu_mem_usage,
        cfg.paged_attn_context_len,
    ) {
        (None, None, None) => MemoryGpuConfig::ContextSize(4096),
        (None, None, Some(ctxt)) => MemoryGpuConfig::ContextSize(ctxt),
        (None, Some(usage), None) => MemoryGpuConfig::Utilization(usage),
        (Some(mem), None, None) => MemoryGpuConfig::MbAmount(mem),
        (Some(_), Some(usage), None) => MemoryGpuConfig::Utilization(usage),
        (Some(_), None, Some(ctxt)) => MemoryGpuConfig::ContextSize(ctxt),
        (None, Some(usage), Some(_)) => MemoryGpuConfig::Utilization(usage),
        (Some(_), Some(usage), Some(_)) => MemoryGpuConfig::Utilization(usage),
    };

    PagedAttentionConfig::new(cfg.paged_attn_block_size, mem_gpu, cache_type)
        .map(Some)
        .map_err(|e| LLMError::InvalidRequest(format!("invalid paged_attn config: {e}")))
}

struct GgufSpec {
    model_id: String,
    files: Vec<String>,
}

fn gguf_spec_from_config(cfg: &MistralRSConfig) -> Result<Option<GgufSpec>, LLMError> {
    let model_ref =
        parse_model_ref(&cfg.model).map_err(|e| LLMError::InvalidRequest(e.to_string()))?;
    match model_ref {
        ModelRef::Hf(model) => Ok(Some(GgufSpec {
            model_id: model.repo,
            files: vec![model.file],
        })),
        ModelRef::LocalPath(path)
            if cfg.model.ends_with(".gguf")
                || path.extension().and_then(|ext| ext.to_str()) == Some("gguf") =>
        {
            let file = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| LLMError::InvalidRequest("invalid gguf file path".into()))?
                .to_string();
            let model_id = path
                .parent()
                .and_then(|parent| parent.to_str())
                .unwrap_or(".")
                .to_string();
            Ok(Some(GgufSpec {
                model_id,
                files: vec![file],
            }))
        }
        _ => Ok(None),
    }
}

async fn build_text_model(cfg: &MistralRSConfig) -> Result<Model, LLMError> {
    let mut builder = TextModelBuilder::new(&cfg.model).with_logging();
    if let Some(token_source) = token_source_override(cfg)? {
        builder = builder.with_token_source(token_source);
    }
    if let Some(revision) = cfg.hf_revision.as_ref() {
        builder = builder.with_hf_revision(revision);
    }
    if let Some(chat_template) = cfg.chat_template.as_ref() {
        builder = builder.with_chat_template(chat_template);
    }
    if let Some(tokenizer_json) = cfg.tokenizer_json.as_ref() {
        builder = builder.with_tokenizer_json(tokenizer_json);
    }
    if let Some(jinja_explicit) = cfg.jinja_explicit.as_ref() {
        builder = builder.with_jinja_explicit(jinja_explicit.clone());
    }
    if let Some(hf_cache_path) = cfg.hf_cache_path.as_ref() {
        builder = builder.from_hf_cache_path(PathBuf::from(hf_cache_path));
    }
    if let Some(loader_type) = text_loader_type(cfg)? {
        builder = builder.with_loader_type(loader_type);
    }
    if let Some(dtype) = dtype_from_config(cfg)? {
        builder = builder.with_dtype(dtype);
    }
    if let Some(topology) = topology_from_config(cfg)? {
        builder = builder.with_topology(topology);
    }
    if let Some(isq) = isq_from_config(cfg)? {
        builder = builder.with_isq(isq);
    }
    if let Some(imatrix) = cfg.imatrix.as_ref() {
        builder = builder.with_imatrix(PathBuf::from(imatrix));
    }
    if let Some(calibration_file) = cfg.calibration_file.as_ref() {
        builder = builder.with_calibration_file(PathBuf::from(calibration_file));
    }
    if let Some(paged_attn_cfg) = paged_attn_config(cfg)? {
        builder = builder.with_paged_attn(paged_attn_cfg);
    }
    if cfg.throughput_logging.unwrap_or(false) {
        builder = builder.with_throughput_logging();
    }
    if cfg.force_cpu.unwrap_or(false) {
        builder = builder.with_force_cpu();
    }
    if let Some(max_num_seqs) = cfg.max_num_seqs {
        builder = builder.with_max_num_seqs(max_num_seqs);
    }
    if cfg.no_kv_cache.unwrap_or(false) {
        builder = builder.with_no_kv_cache();
    }
    if cfg.prefix_cache_n.is_some() {
        builder = builder.with_prefix_cache_n(cfg.prefix_cache_n);
    }
    if let Some(device_map) = device_map_setting(cfg, MistralRSModelKind::Text) {
        builder = builder.with_device_mapping(device_map);
    }

    builder
        .build()
        .await
        .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))
}

async fn build_vision_model(cfg: &MistralRSConfig) -> Result<Model, LLMError> {
    let mut builder = MultimodalModelBuilder::new(&cfg.model).with_logging();
    if let Some(token_source) = token_source_override(cfg)? {
        builder = builder.with_token_source(token_source);
    }
    if let Some(revision) = cfg.hf_revision.as_ref() {
        builder = builder.with_hf_revision(revision);
    }
    if let Some(chat_template) = cfg.chat_template.as_ref() {
        builder = builder.with_chat_template(chat_template);
    }
    if let Some(tokenizer_json) = cfg.tokenizer_json.as_ref() {
        builder = builder.with_tokenizer_json(tokenizer_json);
    }
    if let Some(jinja_explicit) = cfg.jinja_explicit.as_ref() {
        builder = builder.with_jinja_explicit(jinja_explicit.clone());
    }
    if let Some(hf_cache_path) = cfg.hf_cache_path.as_ref() {
        builder = builder.from_hf_cache_path(PathBuf::from(hf_cache_path));
    }
    if let Some(loader_type) = vision_loader_type(cfg)? {
        builder = builder.with_loader_type(loader_type);
    }
    if let Some(dtype) = dtype_from_config(cfg)? {
        builder = builder.with_dtype(dtype);
    }
    if let Some(topology) = topology_from_config(cfg)? {
        builder = builder.with_topology(topology);
    }
    if let Some(isq) = isq_from_config(cfg)? {
        builder = builder.with_isq(isq);
    }
    if let Some(calibration_file) = cfg.calibration_file.as_ref() {
        builder = builder.with_calibration_file(PathBuf::from(calibration_file));
    }
    if let Some(max_edge) = cfg.max_edge {
        builder = builder.with_max_edge(max_edge);
    }
    if let Some(paged_attn_cfg) = paged_attn_config(cfg)? {
        builder = builder.with_paged_attn(paged_attn_cfg);
    }
    if cfg.throughput_logging.unwrap_or(false) {
        builder = builder.with_throughput_logging();
    }
    if cfg.force_cpu.unwrap_or(false) {
        builder = builder.with_force_cpu();
    }
    if let Some(max_num_seqs) = cfg.max_num_seqs {
        builder = builder.with_max_num_seqs(max_num_seqs);
    }
    if cfg.prefix_cache_n.is_some() {
        builder = builder.with_prefix_cache_n(cfg.prefix_cache_n);
    }
    if let Some(device_map) = device_map_setting(cfg, MistralRSModelKind::Vision) {
        builder = builder.with_device_mapping(device_map);
    }

    builder
        .build()
        .await
        .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))
}

async fn build_embedding_model(cfg: &MistralRSConfig) -> Result<Model, LLMError> {
    let mut builder = EmbeddingModelBuilder::new(&cfg.model).with_logging();
    if let Some(token_source) = token_source_override(cfg)? {
        builder = builder.with_token_source(token_source);
    }
    if let Some(revision) = cfg.hf_revision.as_ref() {
        builder = builder.with_hf_revision(revision);
    }
    if let Some(tokenizer_json) = cfg.tokenizer_json.as_ref() {
        builder = builder.with_tokenizer_json(tokenizer_json);
    }
    if let Some(hf_cache_path) = cfg.hf_cache_path.as_ref() {
        builder = builder.from_hf_cache_path(PathBuf::from(hf_cache_path));
    }
    if let Some(loader_type) = embedding_loader_type(cfg)? {
        builder = builder.with_loader_type(loader_type);
    }
    if let Some(dtype) = dtype_from_config(cfg)? {
        builder = builder.with_dtype(dtype);
    }
    if let Some(topology) = topology_from_config(cfg)? {
        builder = builder.with_topology(topology);
    }
    if let Some(isq) = isq_from_config(cfg)? {
        builder = builder.with_isq(isq);
    }
    if paged_attn_config(cfg)?.is_some() {
        return Err(LLMError::InvalidRequest(
            "paged_attn is not supported for embedding models".into(),
        ));
    }
    if cfg.throughput_logging.unwrap_or(false) {
        builder = builder.with_throughput_logging();
    }
    if cfg.force_cpu.unwrap_or(false) {
        builder = builder.with_force_cpu();
    }
    if let Some(max_num_seqs) = cfg.max_num_seqs {
        builder = builder.with_max_num_seqs(max_num_seqs);
    }
    if let Some(device_map) = device_map_setting(cfg, MistralRSModelKind::Embedding) {
        builder = builder.with_device_mapping(device_map);
    }

    builder
        .build()
        .await
        .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))
}

async fn build_gguf_model(cfg: &MistralRSConfig, spec: GgufSpec) -> Result<Model, LLMError> {
    let mut builder = GgufModelBuilder::new(spec.model_id, spec.files).with_logging();
    if let Some(token_source) = token_source_override(cfg)? {
        builder = builder.with_token_source(token_source);
    }
    if let Some(revision) = cfg.hf_revision.as_ref() {
        builder = builder.with_hf_revision(revision);
    }
    if let Some(tok_model_id) = cfg.tok_model_id.as_ref() {
        builder = builder.with_tok_model_id(tok_model_id);
    }
    if let Some(chat_template) = cfg.chat_template.as_ref() {
        builder = builder.with_chat_template(chat_template);
    }
    if let Some(tokenizer_json) = cfg.tokenizer_json.as_ref() {
        builder = builder.with_tokenizer_json(tokenizer_json);
    }
    if let Some(jinja_explicit) = cfg.jinja_explicit.as_ref() {
        builder = builder.with_jinja_explicit(jinja_explicit.clone());
    }
    if let Some(topology) = topology_from_config(cfg)? {
        builder = builder.with_topology(topology);
    }
    if let Some(paged_attn_cfg) = paged_attn_config(cfg)? {
        builder = builder.with_paged_attn(paged_attn_cfg);
    }
    if cfg.throughput_logging.unwrap_or(false) {
        builder = builder.with_throughput_logging();
    }
    if cfg.force_cpu.unwrap_or(false) {
        builder = builder.with_force_cpu();
    }
    if let Some(max_num_seqs) = cfg.max_num_seqs {
        builder = builder.with_max_num_seqs(max_num_seqs);
    }
    if cfg.no_kv_cache.unwrap_or(false) {
        builder = builder.with_no_kv_cache();
    }
    if cfg.prefix_cache_n.is_some() {
        builder = builder.with_prefix_cache_n(cfg.prefix_cache_n);
    }
    if let Some(device_map) = device_map_setting(cfg, MistralRSModelKind::Text) {
        builder = builder.with_device_mapping(device_map);
    }

    builder
        .build()
        .await
        .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))
}

async fn build_speech_model(cfg: &MistralRSConfig) -> Result<Model, LLMError> {
    let loader_type = speech_loader_type(cfg)?;
    let mut builder = SpeechModelBuilder::new(&cfg.model, loader_type).with_logging();
    if let Some(token_source) = token_source_override(cfg)? {
        builder = builder.with_token_source(token_source);
    }
    if let Some(revision) = cfg.hf_revision.as_ref() {
        builder = builder.with_hf_revision(revision);
    }
    if let Some(dac_model_id) = cfg.speech_dac_model_id.as_ref() {
        builder = builder.with_dac_model_id(dac_model_id.clone());
    }
    if let Some(dtype) = dtype_from_config(cfg)? {
        builder = builder.with_dtype(dtype);
    }
    if cfg.force_cpu.unwrap_or(false) {
        builder = builder.with_force_cpu();
    }
    if let Some(max_num_seqs) = cfg.max_num_seqs {
        builder = builder.with_max_num_seqs(max_num_seqs);
    }

    builder
        .build()
        .await
        .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))
}
