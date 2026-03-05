use crate::backend::llama_backend;
use crate::config::LlamaCppConfig;
use crate::context::{
    apply_context_params, estimate_context_memory, resolve_n_batch, resolve_n_ubatch,
};
use crate::multimodal::MultimodalContext;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::mtmd::{MtmdBitmap, MtmdInputChunkType, MtmdInputText};
use querymt::error::LLMError;
use std::num::NonZeroU32;
use std::sync::Arc;

pub(crate) struct ToolPrefillState<'a> {
    pub(crate) ctx: LlamaContext<'a>,
    pub(crate) input_tokens: u32,
    pub(crate) n_cur: i32,
    pub(crate) n_len_total: i32,
    pub(crate) n_batch: usize,
}

/// Build and prefill a llama context for tool generation.
///
/// This helper centralizes prompt prefill so both sync and streaming tool paths
/// share identical context sizing and multimodal behavior.
pub(crate) fn prefill_for_tool_generation<'a>(
    model: &'a Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    prompt: &str,
    max_tokens: u32,
    mm_ctx: Option<&MultimodalContext>,
    bitmaps: &[MtmdBitmap],
) -> Result<ToolPrefillState<'a>, LLMError> {
    if !bitmaps.is_empty() && mm_ctx.is_none() {
        return Err(LLMError::InvalidRequest(
            "Images provided but model does not support multimodal input. \
             Configure mmproj_path or use a vision-capable model."
                .into(),
        ));
    }

    let backend = llama_backend()?;

    if let Some(mm_ctx) = mm_ctx {
        // Multimodal path: tokenize first so n_ctx autosizing is based on true input size.
        let input_text = MtmdInputText {
            text: prompt.to_string(),
            add_special: cfg.add_bos.unwrap_or(true),
            parse_special: true,
        };
        let bitmap_refs: Vec<&MtmdBitmap> = bitmaps.iter().collect();
        let chunks = mm_ctx
            .ctx
            .tokenize(input_text, &bitmap_refs)
            .map_err(|e| LLMError::ProviderError(format!("MTMD tokenization failed: {}", e)))?;

        let input_tokens = chunks.total_tokens() as u32;
        let n_ctx_needed = input_tokens + max_tokens;
        let n_ctx_raw = cfg
            .n_ctx
            .unwrap_or_else(|| n_ctx_needed.min(model.n_ctx_train()));
        let n_ctx = NonZeroU32::new(n_ctx_raw)
            .ok_or_else(|| LLMError::InvalidRequest("n_ctx must be greater than zero".into()))?;

        let n_batch = resolve_n_batch(cfg, n_ctx.get());
        let n_ubatch = resolve_n_ubatch(cfg, n_batch, true);

        let mut ctx_params = LlamaContextParams::default();
        ctx_params = ctx_params.with_n_ctx(Some(n_ctx));
        ctx_params = ctx_params.with_n_batch(n_batch);
        ctx_params = ctx_params.with_n_ubatch(n_ubatch);
        if let Some(n_threads) = cfg.n_threads {
            ctx_params = ctx_params.with_n_threads(n_threads);
        }
        if let Some(n_threads_batch) = cfg.n_threads_batch {
            ctx_params = ctx_params.with_n_threads_batch(n_threads_batch);
        }
        ctx_params = apply_context_params(cfg, ctx_params)?;

        let mut ctx = model.new_context(&*backend, ctx_params).map_err(|e| {
            let est = estimate_context_memory(model, cfg, n_ctx.get());
            LLMError::ProviderError(format!(
                "Failed to create context (n_ctx={}): {}. {}\n\
                 Try reducing n_ctx or using KV cache quantization.",
                n_ctx.get(),
                e,
                est.summary()
            ))
        })?;

        let n_ctx_total = ctx.n_ctx() as i32;
        let n_len_total = input_tokens as i32 + max_tokens as i32;
        if n_len_total > n_ctx_total {
            return Err(LLMError::InvalidRequest(format!(
                "Prompt + max_tokens ({n_len_total}) exceeds context window ({n_ctx_total})"
            )));
        }

        // Vision models decode media chunks non-causally, which requires each media
        // chunk to fit in a single physical micro-batch.
        let n_ubatch = resolve_n_ubatch(cfg, n_batch, true) as usize;
        for i in 0..chunks.len() {
            if let Some(chunk) = chunks.get(i) {
                if chunk.chunk_type() != MtmdInputChunkType::Text {
                    let media_tokens = chunk.n_tokens();
                    if media_tokens > n_ubatch {
                        return Err(LLMError::InvalidRequest(format!(
                            "Image produces {media_tokens} tokens but n_ubatch is {n_ubatch}. \
                             Increase n_batch/n_ubatch or use a lower-resolution image."
                        )));
                    }
                }
            }
        }

        let n_past = chunks
            .eval_chunks(&mm_ctx.ctx, &mut ctx, 0, 0, n_batch as i32, true)
            .map_err(|e| LLMError::ProviderError(format!("MTMD evaluation failed: {}", e)))?;

        return Ok(ToolPrefillState {
            ctx,
            input_tokens,
            n_cur: n_past,
            n_len_total,
            n_batch: n_batch as usize,
        });
    }

    // Text-only path.
    let add_bos = cfg.add_bos.unwrap_or(true);
    let tokens = model
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

    let input_tokens = tokens.len() as u32;
    let n_ctx_needed = input_tokens + max_tokens;
    let n_ctx_raw = cfg
        .n_ctx
        .unwrap_or_else(|| n_ctx_needed.min(model.n_ctx_train()));
    let n_ctx = NonZeroU32::new(n_ctx_raw)
        .ok_or_else(|| LLMError::InvalidRequest("n_ctx must be greater than zero".into()))?;

    let n_batch = resolve_n_batch(cfg, n_ctx.get());
    let n_ubatch = resolve_n_ubatch(cfg, n_batch, false);

    let mut ctx_params = LlamaContextParams::default();
    ctx_params = ctx_params.with_n_ctx(Some(n_ctx));
    ctx_params = ctx_params.with_n_batch(n_batch);
    ctx_params = ctx_params.with_n_ubatch(n_ubatch);
    if let Some(n_threads) = cfg.n_threads {
        ctx_params = ctx_params.with_n_threads(n_threads);
    }
    if let Some(n_threads_batch) = cfg.n_threads_batch {
        ctx_params = ctx_params.with_n_threads_batch(n_threads_batch);
    }
    ctx_params = apply_context_params(cfg, ctx_params)?;

    let mut ctx = model.new_context(&*backend, ctx_params).map_err(|e| {
        let est = estimate_context_memory(model, cfg, n_ctx.get());
        LLMError::ProviderError(format!(
            "Failed to create context (n_ctx={}): {}. {}\n\
             Try reducing n_ctx or using KV cache quantization.",
            n_ctx.get(),
            e,
            est.summary()
        ))
    })?;

    let n_ctx_total = ctx.n_ctx() as i32;
    let n_len_total = tokens.len() as i32 + max_tokens as i32;
    if n_len_total > n_ctx_total {
        return Err(LLMError::InvalidRequest(format!(
            "Prompt + max_tokens ({n_len_total}) exceeds context window ({n_ctx_total})"
        )));
    }

    let mut batch = LlamaBatch::new(n_batch as usize, 1);
    let last_index = tokens.len().saturating_sub(1);
    for chunk_start in (0..tokens.len()).step_by(n_batch as usize) {
        batch.clear();
        let chunk_end = (chunk_start + n_batch as usize).min(tokens.len());
        for i in chunk_start..chunk_end {
            let is_last = i == last_index;
            batch
                .add(tokens[i], i as i32, &[0], is_last)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        }
        ctx.decode(&mut batch).map_err(|e| {
            let est = estimate_context_memory(model, cfg, n_ctx.get());
            LLMError::ProviderError(format!(
                "Failed to decode prompt batch (chunk {}/{}, n_ctx={}): {}. {}",
                chunk_start / n_batch as usize + 1,
                tokens.len().div_ceil(n_batch as usize),
                n_ctx.get(),
                e,
                est.summary()
            ))
        })?;
    }

    Ok(ToolPrefillState {
        ctx,
        input_tokens,
        n_cur: tokens.len() as i32,
        n_len_total,
        n_batch: n_batch as usize,
    })
}
