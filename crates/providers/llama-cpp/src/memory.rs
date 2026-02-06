use llama_cpp_2::context::params::KvCacheType;
use llama_cpp_2::list_llama_ggml_backend_devices;
use querymt::error::LLMError;

/// Estimated memory breakdown for a llama.cpp context.
#[derive(Debug)]
pub(crate) struct MemoryEstimate {
    /// Model weight size in bytes (from model.size()).
    pub(crate) model_bytes: u64,
    /// Estimated KV cache size in bytes.
    pub(crate) kv_cache_bytes: u64,
    /// Overhead estimate for compute scratch buffers (20% of KV cache).
    pub(crate) overhead_bytes: u64,
    /// Total estimated memory in bytes.
    pub(crate) total_bytes: u64,
    /// Best available GPU memory figure (total or free) in bytes, or 0 if unknown.
    pub(crate) gpu_memory_bytes: u64,
    /// Name of the GPU device, if found.
    pub(crate) gpu_name: String,
}

impl MemoryEstimate {
    pub(crate) fn total_gb(&self) -> f64 {
        self.total_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    }

    pub(crate) fn gpu_gb(&self) -> f64 {
        self.gpu_memory_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    }

    pub(crate) fn model_gb(&self) -> f64 {
        self.model_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    }

    pub(crate) fn kv_cache_gb(&self) -> f64 {
        self.kv_cache_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    }

    /// Build a human-readable summary for log messages and errors.
    pub(crate) fn summary(&self) -> String {
        let mut s = format!(
            "Memory estimate: model={:.1}GB, kv_cache={:.1}GB, overhead={:.1}GB, total={:.1}GB",
            self.model_gb(),
            self.kv_cache_gb(),
            self.overhead_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
            self.total_gb(),
        );
        if self.gpu_memory_bytes > 0 {
            s.push_str(&format!(", gpu={}({:.1}GB)", self.gpu_name, self.gpu_gb(),));
        }
        s
    }

    /// Build actionable suggestions when memory is tight/exceeded.
    pub(crate) fn suggestions(n_ctx: u32, kv_type_configured: bool, fa_configured: bool) -> String {
        let mut suggestions = Vec::new();
        suggestions.push(format!(
            "- Reduce n_ctx (current: {}). Try n_ctx={} or n_ctx={}",
            n_ctx,
            (n_ctx / 2).max(2048),
            (n_ctx / 4).max(2048),
        ));
        if !kv_type_configured {
            suggestions.push(
                "- Enable KV cache quantization: kv_cache_type_k=q4_0 kv_cache_type_v=q4_0 \
                 (reduces KV cache to ~1/4 of f16)"
                    .to_string(),
            );
        }
        if !fa_configured {
            suggestions.push(
                "- Enable flash attention: flash_attention=enabled \
                 (required for KV quantization, also reduces memory)"
                    .to_string(),
            );
        }
        suggestions
            .push("- Use a smaller model quantization (e.g. Q4_K_M instead of Q8_0)".to_string());
        suggestions.push(
            "- Reduce n_gpu_layers to offload fewer layers to GPU (uses slower CPU for some layers)"
                .to_string(),
        );
        suggestions.join("\n")
    }
}

/// Bytes per element for KV cache quantization types.
///
/// These are approximate — quantized types use block-based encoding, so the
/// actual per-element cost is the block size divided by elements-per-block.
pub(crate) fn kv_cache_bytes_per_element(type_str: Option<&str>) -> f64 {
    match type_str.map(|s| s.to_lowercase()).as_deref() {
        Some("f32") => 4.0,
        Some("f16") | None => 2.0, // f16 is the default
        Some("bf16") => 2.0,
        Some("q8_0") => 1.0625, // 32 bytes per block of 32 elements + 2-byte scale
        Some("q4_0") => 0.5625, // 16 bytes per block of 32 elements + 2-byte scale
        Some("q4_1") => 0.625,  // 16 bytes + 2 scales per block of 32
        Some("q5_0") => 0.6875,
        Some("q5_1") => 0.75,
        _ => 2.0, // unknown → assume f16
    }
}

/// Query the best available GPU device memory.
///
/// Returns `(total_bytes, free_bytes, device_name)`.
/// If no GPU device is found, returns `(0, 0, "none")`.
pub(crate) fn query_gpu_memory() -> (u64, u64, String) {
    let devices = list_llama_ggml_backend_devices();
    // Prefer Metal > CUDA > Vulkan > any GPU
    for preferred_backend in &["Metal", "CUDA", "Vulkan"] {
        if let Some(dev) = devices.iter().find(|d| d.backend == *preferred_backend) {
            return (
                dev.memory_total as u64,
                dev.memory_free as u64,
                format!("{} ({})", dev.name, dev.description),
            );
        }
    }
    // Fallback: any non-CPU device
    if let Some(dev) = devices
        .iter()
        .find(|d| !matches!(d.device_type, llama_cpp_2::LlamaBackendDeviceType::Cpu))
    {
        return (
            dev.memory_total as u64,
            dev.memory_free as u64,
            format!("{} ({})", dev.name, dev.description),
        );
    }
    (0, 0, "none".to_string())
}

/// Parse a KV cache type string into the corresponding `KvCacheType` enum.
///
/// Supports the most commonly useful quantization types for KV cache.
pub(crate) fn parse_kv_cache_type(s: &str) -> Result<KvCacheType, LLMError> {
    match s.to_lowercase().as_str() {
        "f32" => Ok(KvCacheType::F32),
        "f16" => Ok(KvCacheType::F16),
        "bf16" => Ok(KvCacheType::BF16),
        "q8_0" => Ok(KvCacheType::Q8_0),
        "q4_0" => Ok(KvCacheType::Q4_0),
        "q4_1" => Ok(KvCacheType::Q4_1),
        "q5_0" => Ok(KvCacheType::Q5_0),
        "q5_1" => Ok(KvCacheType::Q5_1),
        other => Err(LLMError::InvalidRequest(format!(
            "Unsupported KV cache type: '{}'. Supported types: f32, f16, bf16, q8_0, q4_0, q4_1, q5_0, q5_1",
            other
        ))),
    }
}
