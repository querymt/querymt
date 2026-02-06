use crate::config::{
    FlashAttentionPolicy, LlamaCppConfig, LLAMA_FLASH_ATTN_TYPE_AUTO,
    LLAMA_FLASH_ATTN_TYPE_DISABLED, LLAMA_FLASH_ATTN_TYPE_ENABLED,
};
use crate::memory::{
    kv_cache_bytes_per_element, parse_kv_cache_type, query_gpu_memory, MemoryEstimate,
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

/// Resolve the effective `n_batch` value.
///
/// Returns:
/// - `cfg.n_batch` if the user explicitly configured it.
/// - Otherwise `min(n_ctx, DEFAULT_N_BATCH_CAP)` so we never ask Metal (or
///   any other backend) to process an unreasonably large batch at once.
pub(crate) fn resolve_n_batch(cfg: &LlamaCppConfig, n_ctx: u32) -> u32 {
    cfg.n_batch
        .unwrap_or_else(|| n_ctx.min(DEFAULT_N_BATCH_CAP))
}

/// Apply flash attention and KV cache quantization settings to context params.
///
/// This is called from all context creation sites to ensure consistent behavior.
pub(crate) fn apply_context_params(
    cfg: &LlamaCppConfig,
    mut ctx_params: LlamaContextParams,
) -> Result<LlamaContextParams, LLMError> {
    // Flash attention
    if let Some(ref policy) = cfg.flash_attention {
        let fa_type = match policy {
            FlashAttentionPolicy::Auto => LLAMA_FLASH_ATTN_TYPE_AUTO,
            FlashAttentionPolicy::Enabled => LLAMA_FLASH_ATTN_TYPE_ENABLED,
            FlashAttentionPolicy::Disabled => LLAMA_FLASH_ATTN_TYPE_DISABLED,
        };
        ctx_params = ctx_params.with_flash_attention_policy(fa_type);
    }

    // KV cache quantization for keys
    if let Some(ref type_k) = cfg.kv_cache_type_k {
        if cfg.flash_attention.is_none() {
            log::warn!(
                "kv_cache_type_k='{}' is set but flash_attention is not configured. \
                 KV cache quantization requires flash attention to be enabled.",
                type_k
            );
        }
        ctx_params = ctx_params.with_type_k(parse_kv_cache_type(type_k)?);
    }

    // KV cache quantization for values
    if let Some(ref type_v) = cfg.kv_cache_type_v {
        if cfg.flash_attention.is_none() {
            log::warn!(
                "kv_cache_type_v='{}' is set but flash_attention is not configured. \
                 KV cache quantization requires flash attention to be enabled.",
                type_v
            );
        }
        ctx_params = ctx_params.with_type_v(parse_kv_cache_type(type_v)?);
    }

    Ok(ctx_params)
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
    let head_dim = if n_head > 0 { n_embd / n_head } else { 128 };

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
