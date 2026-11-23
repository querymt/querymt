use crate::backend::llama_backend;
use crate::config::LlamaCppConfig;
use crate::context::{
    apply_context_params, estimate_context_memory, resolve_n_batch, resolve_n_ubatch,
};
use crate::lora::{Models, new_target_context};
use crate::multimodal::MediaInputs;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::mtmd::{MtmdBitmap, MtmdInputChunkType, MtmdInputText};
use querymt::error::LLMError;
use std::num::NonZeroU32;

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
    models: Models<'a>,
    cfg: &LlamaCppConfig,
    prompt: &str,
    max_tokens: u32,
    media: MediaInputs<'_>,
) -> Result<ToolPrefillState<'a>, LLMError> {
    let model = models.target;
    let MediaInputs {
        ctx: mm_ctx,
        bitmaps,
    } = media;
    if !bitmaps.is_empty() && mm_ctx.is_none() {
        return Err(LLMError::InvalidRequest(
            "Images provided but model does not support multimodal input. \
             Configure mmproj_path or use a vision-capable model."
                .into(),
        ));
    }

    let backend = llama_backend()?;

    if let Some(mm_ctx) = mm_ctx.filter(|_| !bitmaps.is_empty()) {
        // Multimodal path: tokenize first so n_ctx autosizing is based on true input size.
        let input_text = MtmdInputText {
            text: prompt.to_string(),
            add_special: cfg.add_bos.unwrap_or(true),
            parse_special: true,
        };
        let bitmap_refs: Vec<&MtmdBitmap> = bitmaps.iter().collect();
        let mut mm_ctx = mm_ctx.lock()?;
        let chunks = mm_ctx
            .tokenize(input_text, &bitmap_refs)
            .map_err(|e| LLMError::ProviderError(format!("MTMD tokenization failed: {}", e)))?;

        let (input_tokens, n_len_total, n_ctx) = prefill_bounds(
            chunks.total_tokens(),
            max_tokens,
            cfg.n_ctx,
            model.n_ctx_train(),
        )?;

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

        let mut ctx =
            new_target_context(models, &backend, ctx_params, None, &cfg.lora).map_err(|err| {
                err.into_llm_error(|e| {
                    let est = estimate_context_memory(model, cfg, n_ctx.get());
                    LLMError::ProviderError(format!(
                        "Failed to create context (n_ctx={}): {}. {}\n\
                         Try reducing n_ctx or using KV cache quantization.",
                        n_ctx.get(),
                        e,
                        est.summary()
                    ))
                })
            })?;

        check_context_fit(n_len_total.cast_unsigned(), ctx.n_ctx())?;

        // Vision models decode media chunks non-causally, which requires each media
        // chunk to fit in a single physical micro-batch.
        let n_ubatch = resolve_n_ubatch(cfg, n_batch, true) as usize;
        for i in 0..chunks.len() {
            if let Some(chunk) = chunks.get(i)
                && chunk.chunk_type() != MtmdInputChunkType::Text
            {
                let media_tokens = chunk.n_tokens();
                if media_tokens > n_ubatch {
                    return Err(LLMError::InvalidRequest(format!(
                        "Image produces {media_tokens} tokens but n_ubatch is {n_ubatch}. \
                         Increase n_batch/n_ubatch or use a lower-resolution image."
                    )));
                }
            }
        }

        let n_past = chunks
            .eval_chunks(&mut mm_ctx, &mut ctx, 0, 0, n_batch as i32, true)
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
    let tokens = model.vocab().tokenize(prompt.as_bytes(), add_bos, true);

    if tokens.is_empty() {
        return Err(LLMError::InvalidRequest(
            "Prompt tokenization resulted in an empty sequence".into(),
        ));
    }

    let (input_tokens, n_len_total, n_ctx) =
        prefill_bounds(tokens.len(), max_tokens, cfg.n_ctx, model.n_ctx_train())?;

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

    let mut ctx =
        new_target_context(models, &backend, ctx_params, None, &cfg.lora).map_err(|err| {
            err.into_llm_error(|e| {
                let est = estimate_context_memory(model, cfg, n_ctx.get());
                LLMError::ProviderError(format!(
                    "Failed to create context (n_ctx={}): {}. {}\n\
                     Try reducing n_ctx or using KV cache quantization.",
                    n_ctx.get(),
                    e,
                    est.summary()
                ))
            })
        })?;

    check_context_fit(n_len_total.cast_unsigned(), ctx.n_ctx())?;

    let mut batch = LlamaBatch::new(n_batch as usize, 1);
    let last_index = tokens.len().saturating_sub(1);
    for chunk_start in (0..tokens.len()).step_by(n_batch as usize) {
        batch.clear();
        let chunk_end = (chunk_start + n_batch as usize).min(tokens.len());
        for (i, &token) in tokens.iter().enumerate().take(chunk_end).skip(chunk_start) {
            let is_last = i == last_index;
            batch
                .add(token, i as i32, &[0], is_last)
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

// Both tokenizers feed this check before allocating a native context.
fn prefill_bounds(
    input: usize,
    output: u32,
    configured_ctx: Option<u32>,
    trained_ctx: u32,
) -> Result<(u32, i32, NonZeroU32), LLMError> {
    let input = u32::try_from(input)
        .map_err(|_| LLMError::InvalidRequest("prompt token count exceeds u32".into()))?;
    let (needed, positions) = input
        .checked_add(output)
        .and_then(|needed| Some((needed, i32::try_from(needed).ok()?)))
        .ok_or_else(|| {
            LLMError::InvalidRequest(
                "prompt and output token count exceeds supported positions".into(),
            )
        })?;
    let n_ctx = NonZeroU32::new(configured_ctx.unwrap_or(needed.min(trained_ctx)))
        .ok_or_else(|| LLMError::InvalidRequest("n_ctx must be greater than zero".into()))?;
    check_context_fit(needed, n_ctx.get())?;
    Ok((input, positions, n_ctx))
}

fn check_context_fit(needed: u32, n_ctx: u32) -> Result<(), LLMError> {
    if needed > n_ctx {
        return Err(LLMError::InvalidRequest(format!(
            "Prompt + max_tokens ({needed}) exceeds context window ({n_ctx})"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefill_checks_positions_and_window_before_allocating_context() {
        let (input, positions, window) = prefill_bounds(512, 128, None, 1024).unwrap();
        assert_eq!((input, positions, window.get()), (512, 640, 640));
        assert!(matches!(
            prefill_bounds(512, 128, None, 639),
            Err(LLMError::InvalidRequest(_))
        ));
        assert!(matches!(
            prefill_bounds(512, 128, Some(639), 1024),
            Err(LLMError::InvalidRequest(_))
        ));
        assert!(matches!(
            prefill_bounds(512, i32::MAX as u32, None, 1024),
            Err(LLMError::InvalidRequest(_))
        ));
    }
}
