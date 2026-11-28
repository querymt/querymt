use crate::config::{
    FlashAttentionPolicy, LLAMA_FLASH_ATTN_TYPE_AUTO, LLAMA_FLASH_ATTN_TYPE_DISABLED,
    LLAMA_FLASH_ATTN_TYPE_ENABLED, LlamaCppConfig,
};
use crate::memory::{
    MemoryEstimate, kv_cache_bytes_per_element, parse_kv_cache_type, query_gpu_memory,
};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::model::LlamaModel;
use querymt::error::LLMError;
use std::sync::Arc;

/// The maximum batch size to use when the user has not configured `n_batch`.
///
/// Setting `n_batch = n_ctx` can exceed Metal GPU command-buffer limits for
/// large context windows (e.g. 80 000 tokens), causing a fatal error in
/// `ggml_metal_synchronize`.  Instead we cap at a sensible default that
/// matches the llama.cpp server.  llama.cpp will automatically chunk prompt
/// processing into multiple decode calls when the prompt is larger than
/// `n_batch`.
pub(crate) const DEFAULT_N_BATCH_CAP: u32 = 4096;

/// The config values that shape a llama.cpp context.
///
/// This is the single place that reads them: every context the provider
/// creates (generation, prefill, MTP) and the memory fit that runs at model
/// load take their batch sizes, flash attention, KV cache and SWA settings
/// from here. A new setting therefore has to be added to this struct to take
/// effect, and so becomes part of the model cache key, where the fit result
/// depends on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContextSettings {
    /// Configured context size. `None` leaves the choice to llama.cpp or a fit.
    pub n_ctx: Option<u32>,
    n_batch: Option<u32>,
    n_ubatch: Option<u32>,
    flash_attention: Option<FlashAttentionPolicy>,
    kv_cache_type_k: Option<String>,
    kv_cache_type_v: Option<String>,
    swa_full: Option<bool>,
}

impl ContextSettings {
    pub(crate) fn from_config(cfg: &LlamaCppConfig) -> Self {
        Self {
            n_ctx: cfg.n_ctx,
            n_batch: cfg.n_batch,
            n_ubatch: cfg.n_ubatch,
            flash_attention: cfg.flash_attention,
            kv_cache_type_k: cfg.kv_cache_type_k.clone(),
            kv_cache_type_v: cfg.kv_cache_type_v.clone(),
            swa_full: cfg.swa_full,
        }
    }

    /// The effective `n_batch` for a context of `n_ctx` tokens.
    ///
    /// Returns:
    /// - the configured `n_batch`, if any;
    /// - otherwise `min(n_ctx, DEFAULT_N_BATCH_CAP)` so we never ask Metal (or
    ///   any other backend) to process an unreasonably large batch at once.
    pub(crate) fn n_batch(&self, n_ctx: u32) -> u32 {
        self.n_batch
            .unwrap_or_else(|| n_ctx.min(DEFAULT_N_BATCH_CAP))
    }

    /// The effective `n_ubatch` (physical micro-batch).
    ///
    /// `n_ubatch` is the physical batch size used inside a single
    /// `llama_decode` call. For text-only models the default of 512 is fine
    /// because tokens are processed causally and can be split across multiple
    /// ubatches.
    ///
    /// Vision models (Qwen-VL, LLaVA, etc.) switch to **non-causal** attention
    /// while decoding image embeddings. In non-causal mode llama.cpp requires
    /// that *all* image tokens fit inside a single ubatch. A screenshot at
    /// typical resolution produces ~3 000 tokens for Qwen3-VL, far exceeding
    /// the 512 default.
    ///
    /// Strategy (first match wins):
    /// 1. Explicit `n_ubatch` from config.
    /// 2. If a multimodal projection is active: use `n_batch`, guaranteeing
    ///    that any image that fits in the logical batch also fits in one
    ///    ubatch.
    /// 3. Default (llama.cpp built-in): 512.
    pub(crate) fn n_ubatch(&self, n_batch: u32, has_multimodal: bool) -> u32 {
        if let Some(n_ubatch) = self.n_ubatch {
            return n_ubatch;
        }
        if has_multimodal {
            // Match n_batch so non-causal image decoding never hits the
            // "n_ubatch >= n_tokens" assertion in llama_decode.
            n_batch
        } else {
            // llama.cpp's own default, spelled out so the value is visible.
            512
        }
    }

    /// Apply flash attention, KV cache quantization and SWA settings to
    /// context params.
    ///
    /// Called from all context creation sites to ensure consistent behavior.
    pub(crate) fn apply(
        &self,
        mut ctx_params: LlamaContextParams,
    ) -> Result<LlamaContextParams, LLMError> {
        // Flash attention
        if let Some(policy) = self.flash_attention {
            let fa_type = match policy {
                FlashAttentionPolicy::Auto => LLAMA_FLASH_ATTN_TYPE_AUTO,
                FlashAttentionPolicy::Enabled => LLAMA_FLASH_ATTN_TYPE_ENABLED,
                FlashAttentionPolicy::Disabled => LLAMA_FLASH_ATTN_TYPE_DISABLED,
            };
            ctx_params = ctx_params.with_flash_attention_policy(fa_type);
        }

        // KV cache quantization for keys
        if let Some(type_k) = &self.kv_cache_type_k {
            if self.flash_attention.is_none() {
                log::warn!(
                    "kv_cache_type_k='{}' is set but flash_attention is not configured. \
                     KV cache quantization requires flash attention to be enabled.",
                    type_k
                );
            }
            ctx_params = ctx_params.with_type_k(parse_kv_cache_type(type_k)?);
        }

        // KV cache quantization for values
        if let Some(type_v) = &self.kv_cache_type_v {
            if self.flash_attention.is_none() {
                log::warn!(
                    "kv_cache_type_v='{}' is set but flash_attention is not configured. \
                     KV cache quantization requires flash attention to be enabled.",
                    type_v
                );
            }
            ctx_params = ctx_params.with_type_v(parse_kv_cache_type(type_v)?);
        }

        // llama.cpp's own default is a full-size SWA cache; its tools
        // (llama-cli, llama-server) turn that off. Keep the tools' default: the
        // full-size cache can exhaust GPU memory on sliding-window models at
        // long contexts.
        Ok(ctx_params.with_swa_full(self.swa_full.unwrap_or(false)))
    }

    /// Context parameters that a model fit measures memory against.
    ///
    /// This is the context of a text request, the one that lives as long as
    /// the model does: the same batch sizes, flash attention, KV cache and SWA
    /// settings generation uses. A context size that is not configured stays
    /// `0` so that the fit picks it; the batch size then is the provider
    /// default for a context of at least `DEFAULT_N_BATCH_CAP` tokens.
    ///
    /// A request with images creates its own context with `n_ubatch` raised
    /// to `n_batch` (see [`Self::n_ubatch`]), which needs a larger compute
    /// buffer that the fit does not reserve; leave headroom with
    /// `fit_margin_mib` when sending large images.
    pub(crate) fn fit_params(&self) -> Result<LlamaContextParams, LLMError> {
        let mut params = LlamaContextParams::default();
        let n_batch = match self.n_ctx {
            Some(n_ctx) => {
                let n_ctx = std::num::NonZeroU32::new(n_ctx).ok_or_else(|| {
                    LLMError::InvalidRequest("n_ctx must be greater than zero".into())
                })?;
                params = params.with_n_ctx(Some(n_ctx));
                self.n_batch(n_ctx.get())
            }
            None => {
                // llama.cpp's default is 512; the fit only picks a size from 0.
                params = params.with_n_ctx(None);
                self.n_batch.unwrap_or(DEFAULT_N_BATCH_CAP)
            }
        };
        self.apply(
            params
                .with_n_batch(n_batch)
                .with_n_ubatch(self.n_ubatch(n_batch, false)),
        )
    }
}

/// Convert a failed `llama_decode` into a provider error.
///
/// llama.cpp reports `-2` (allocation failed) and `-3` (compute failed) when
/// the device ran out of memory, which is by far their most common cause. The
/// bare code says nothing about that, and llama.cpp's own message is hidden
/// unless `log` is set, so say what to try.
pub(crate) fn decode_error(error: llama_cpp_2::DecodeError) -> LLMError {
    use llama_cpp_2::DecodeError;
    match error {
        DecodeError::Unknown(code @ (-2 | -3)) => LLMError::ProviderError(format!(
            "Decode failed (llama.cpp status {code}): the device most likely ran out of memory. \
             Try a smaller n_ctx or n_batch, kv_cache_type_k/kv_cache_type_v = \"q8_0\" with \
             flash_attention = \"enabled\", fewer n_gpu_layers or fit = true; set log = \"stderr\" \
             for llama.cpp's own message."
        )),
        other => LLMError::ProviderError(other.to_string()),
    }
}

/// Estimate memory requirements for a given context size.
///
/// Returns a `MemoryEstimate` with model size, estimated KV cache, overhead,
/// and available GPU memory for comparison.
pub(crate) fn estimate_context_memory(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    n_ctx: u32,
) -> MemoryEstimate {
    let n_layer = model.n_layer() as u64;
    let n_head = model.n_head() as u64;
    let n_head_kv = model.n_head_kv() as u64;
    let n_embd = model.n_embd() as u64;
    let head_dim = n_embd.checked_div(n_head).unwrap_or(128);

    let bytes_per_elem_k = kv_cache_bytes_per_element(cfg.kv_cache_type_k.as_deref());
    let bytes_per_elem_v = kv_cache_bytes_per_element(cfg.kv_cache_type_v.as_deref());

    // KV cache: for each layer, we store K and V tensors
    // K: n_head_kv × head_dim × n_ctx × bytes_per_elem_k
    // V: n_head_kv × head_dim × n_ctx × bytes_per_elem_v
    let k_bytes = (n_head_kv * head_dim * n_ctx as u64) as f64 * bytes_per_elem_k;
    let v_bytes = (n_head_kv * head_dim * n_ctx as u64) as f64 * bytes_per_elem_v;
    let kv_cache_bytes = ((k_bytes + v_bytes) * n_layer as f64) as u64;

    let model_bytes = model.size();

    // Overhead: compute scratch buffers, attention scores, logit buffers, etc.
    // Typically 15-25% of KV cache; we use 20% + a 256MB fixed floor.
    let overhead_bytes = (kv_cache_bytes as f64 * 0.20) as u64 + 256 * 1024 * 1024;

    let total_bytes = model_bytes + kv_cache_bytes + overhead_bytes;

    let (gpu_total, gpu_free, gpu_name) = query_gpu_memory();
    // Use free memory if available and nonzero, otherwise total
    let gpu_memory_bytes = if gpu_free > 0 { gpu_free } else { gpu_total };

    MemoryEstimate {
        model_bytes,
        kv_cache_bytes,
        overhead_bytes,
        total_bytes,
        gpu_memory_bytes,
        gpu_name,
    }
}

/// Pre-flight memory check before context creation.
///
/// Logs warnings or returns an error if the estimated memory usage is likely
/// to exceed available GPU memory. This prevents fatal `GGML_ABORT` crashes
/// from Metal/CUDA backends that cannot be caught from Rust.
#[allow(dead_code)]
pub(crate) fn check_memory(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    n_ctx: u32,
    caller: &str,
) -> Result<MemoryEstimate, LLMError> {
    let estimate = estimate_context_memory(model, cfg, n_ctx);

    log::debug!(
        "[{}] n_ctx={}, n_layer={}, n_head_kv={}, head_dim={}: {}",
        caller,
        n_ctx,
        model.n_layer(),
        model.n_head_kv(),
        if model.n_head() > 0 {
            model.n_embd() as u32 / model.n_head()
        } else {
            128
        },
        estimate.summary(),
    );

    if estimate.gpu_memory_bytes == 0 {
        // Can't determine GPU memory — just log the estimate and proceed
        log::info!(
            "[{}] Cannot determine GPU memory. {}",
            caller,
            estimate.summary()
        );
        return Ok(estimate);
    }

    let ratio = estimate.total_bytes as f64 / estimate.gpu_memory_bytes as f64;

    if ratio > 1.0 {
        // Exceeds available memory — this will almost certainly crash
        let suggestions = MemoryEstimate::suggestions(
            n_ctx,
            cfg.kv_cache_type_k.is_some() || cfg.kv_cache_type_v.is_some(),
            cfg.flash_attention.is_some(),
        );
        let msg = format!(
            "Estimated memory ({:.1}GB) exceeds available GPU memory ({:.1}GB on {}). \
             This will likely cause a fatal GPU error.\n\
             {}\n\
             Suggestions to reduce memory usage:\n{}",
            estimate.total_gb(),
            estimate.gpu_gb(),
            estimate.gpu_name,
            estimate.summary(),
            suggestions,
        );
        log::error!("[{}] {}", caller, msg);
        return Err(LLMError::ProviderError(msg));
    } else if ratio > 0.85 {
        // Tight — warn but proceed
        let suggestions = MemoryEstimate::suggestions(
            n_ctx,
            cfg.kv_cache_type_k.is_some() || cfg.kv_cache_type_v.is_some(),
            cfg.flash_attention.is_some(),
        );
        log::warn!(
            "[{}] Memory usage is tight: estimated {:.1}GB of {:.1}GB available ({:.0}%). \
             The GPU may run out of memory during inference.\n{}\nSuggestions:\n{}",
            caller,
            estimate.total_gb(),
            estimate.gpu_gb(),
            ratio * 100.0,
            estimate.summary(),
            suggestions,
        );
    } else {
        log::info!(
            "[{}] Memory check OK: estimated {:.1}GB of {:.1}GB available ({:.0}%)",
            caller,
            estimate.total_gb(),
            estimate.gpu_gb(),
            ratio * 100.0,
        );
    }

    Ok(estimate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use llama_cpp_2::DecodeError;

    fn cfg() -> LlamaCppConfig {
        LlamaCppConfig {
            model: "m.gguf".into(),
            ..Default::default()
        }
    }

    #[test]
    fn swa_cache_is_windowed_by_default() {
        // llama.cpp's own default is a full-size SWA cache, which runs sliding
        // window models out of GPU memory at long contexts.
        assert!(LlamaContextParams::default().swa_full());
        let params = ContextSettings::from_config(&cfg())
            .apply(LlamaContextParams::default())
            .unwrap();
        assert!(!params.swa_full());
    }

    #[test]
    fn swa_full_can_be_enabled() {
        let mut cfg = cfg();
        cfg.swa_full = Some(true);
        let params = ContextSettings::from_config(&cfg)
            .apply(LlamaContextParams::default())
            .unwrap();
        assert!(params.swa_full());
    }

    #[test]
    fn fit_params_mirror_the_generation_context() {
        let mut cfg = cfg();
        cfg.n_ctx = Some(120_000);
        cfg.kv_cache_type_k = Some("q8_0".into());
        cfg.kv_cache_type_v = Some("q8_0".into());
        cfg.flash_attention = Some(FlashAttentionPolicy::Enabled);
        let params = ContextSettings::from_config(&cfg).fit_params().unwrap();
        assert_eq!(params.n_ctx().map(|n| n.get()), Some(120_000));
        assert_eq!(params.n_batch(), DEFAULT_N_BATCH_CAP);
        assert_eq!(params.n_ubatch(), 512);
        assert!(!params.swa_full());
    }

    #[test]
    fn fit_leaves_an_unset_context_for_the_fit_to_pick() {
        let params = ContextSettings::from_config(&cfg()).fit_params().unwrap();
        assert_eq!(params.n_ctx(), None);
        assert_eq!(params.n_batch(), DEFAULT_N_BATCH_CAP);

        let mut cfg = cfg();
        cfg.n_ctx = Some(0);
        assert!(
            ContextSettings::from_config(&cfg).fit_params().is_err(),
            "n_ctx = 0 is invalid"
        );
    }

    /// A config with every context-shaping value set to a non-default.
    fn tuned_cfg() -> LlamaCppConfig {
        let mut cfg = cfg();
        cfg.n_ctx = Some(65_536);
        cfg.n_batch = Some(1024);
        cfg.n_ubatch = Some(256);
        cfg.flash_attention = Some(FlashAttentionPolicy::Enabled);
        cfg.kv_cache_type_k = Some("q8_0".into());
        cfg.kv_cache_type_v = Some("q4_0".into());
        cfg.swa_full = Some(true);
        cfg
    }

    #[test]
    fn fit_measures_the_context_generation_creates() {
        // The fit and a generation context are both built from the same
        // settings, so every value that shapes the context must agree.
        let settings = ContextSettings::from_config(&tuned_cfg());
        let n_ctx = settings.n_ctx.unwrap();
        let generation = settings
            .apply(
                LlamaContextParams::default()
                    .with_n_ctx(std::num::NonZeroU32::new(n_ctx))
                    .with_n_batch(settings.n_batch(n_ctx))
                    .with_n_ubatch(settings.n_ubatch(settings.n_batch(n_ctx), false)),
            )
            .unwrap();
        let fit = settings.fit_params().unwrap();

        assert_eq!(fit.n_ctx(), generation.n_ctx());
        assert_eq!(fit.n_batch(), generation.n_batch());
        assert_eq!(fit.n_ubatch(), generation.n_ubatch());
        assert_eq!(fit.swa_full(), generation.swa_full());
        assert_eq!(fit.type_k(), generation.type_k());
        assert_eq!(fit.type_v(), generation.type_v());
        assert_eq!(
            fit.flash_attention_policy(),
            generation.flash_attention_policy()
        );
        // ...and they really are the configured, non-default values.
        assert_eq!(fit.n_batch(), 1024);
        assert_eq!(fit.n_ubatch(), 256);
        assert!(fit.swa_full());
        assert_ne!(
            fit.flash_attention_policy(),
            LlamaContextParams::default().flash_attention_policy(),
            "flash attention was set, not left at the default"
        );
    }

    #[test]
    fn every_context_setting_distinguishes_the_settings() {
        // These are part of the model cache key: a change must reload the model.
        let base = ContextSettings::from_config(&tuned_cfg());
        let changed: [fn(&mut LlamaCppConfig); 7] = [
            |c| c.n_ctx = Some(8192),
            |c| c.n_batch = Some(512),
            |c| c.n_ubatch = Some(128),
            |c| c.flash_attention = Some(FlashAttentionPolicy::Disabled),
            |c| c.kv_cache_type_k = Some("f16".into()),
            |c| c.kv_cache_type_v = Some("f16".into()),
            |c| c.swa_full = Some(false),
        ];
        for (i, edit) in changed.into_iter().enumerate() {
            let mut cfg = tuned_cfg();
            edit(&mut cfg);
            assert_ne!(base, ContextSettings::from_config(&cfg), "field #{i}");
        }
    }

    #[test]
    fn the_fit_measures_a_text_context() {
        // Image requests create their own, larger context; the fit sizes the
        // long-lived text one.
        let settings = ContextSettings::from_config(&cfg());
        assert_eq!(settings.fit_params().unwrap().n_ubatch(), 512);
        assert_eq!(settings.n_ubatch(4096, true), 4096, "image context");
        let mut cfg = cfg();
        cfg.n_ubatch = Some(2048);
        let settings = ContextSettings::from_config(&cfg);
        assert_eq!(settings.fit_params().unwrap().n_ubatch(), 2048);
    }

    #[test]
    fn out_of_memory_decode_errors_say_what_to_try() {
        for code in [-2, -3] {
            let message = decode_error(DecodeError::Unknown(code)).to_string();
            assert!(message.contains("out of memory"), "{message}");
            assert!(message.contains(&code.to_string()), "{message}");
            assert!(message.contains("n_ctx"), "{message}");
        }
    }

    #[test]
    fn other_decode_errors_are_passed_through() {
        let message = decode_error(DecodeError::NoKvCacheSlot).to_string();
        assert!(message.contains("NoKvCacheSlot"), "{message}");
        assert!(!message.contains("out of memory"), "{message}");
        let message = decode_error(DecodeError::Unknown(-9)).to_string();
        assert!(!message.contains("out of memory"), "{message}");
    }
}
