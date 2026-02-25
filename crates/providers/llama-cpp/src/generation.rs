use crate::backend::llama_backend;
use crate::config::LlamaCppConfig;
use crate::context::{
    DEFAULT_N_BATCH_CAP, apply_context_params, estimate_context_memory, resolve_n_batch,
    resolve_n_ubatch,
};
use crate::messages;
use crate::multimodal::MultimodalContext;
use crate::response::GeneratedText;
use crate::tools::sampler::{build_fallback_sampler, build_standard_sampler};
use futures::channel::mpsc;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::{AddBos, ChatTemplateResult, LlamaChatMessage, LlamaModel};
use llama_cpp_2::mtmd::{MtmdBitmap, MtmdInputChunkType, MtmdInputText};
use querymt::Usage;
use querymt::chat::ChatMessage;
use querymt::error::LLMError;
use std::num::NonZeroU32;
use std::sync::Arc;

/// Build a prompt from chat messages using optional chat template.
pub(crate) fn build_prompt_with(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
    use_chat_template: bool,
    media_marker: Option<&str>,
) -> Result<(String, bool), LLMError> {
    if !use_chat_template {
        let prompt = messages::messages_to_text(cfg, messages)?;
        return Ok((prompt, false));
    }

    // Try to use the JSON-based chat template approach for better consistency
    // with tool-aware conversations
    let (messages_json, _media_count) = messages::messages_to_json(cfg, messages, media_marker)?;
    let json_messages: Vec<serde_json::Value> = serde_json::from_str(&messages_json)
        .map_err(|e| LLMError::ProviderError(format!("Failed to parse messages JSON: {}", e)))?;

    // Convert JSON messages to LlamaChatMessage format
    let mut chat_messages = Vec::new();
    for json_msg in json_messages {
        let role = json_msg["role"]
            .as_str()
            .ok_or_else(|| LLMError::ProviderError("Missing role in message".into()))?
            .to_string();
        let content = json_msg["content"].as_str().unwrap_or("").to_string();
        chat_messages.push(
            LlamaChatMessage::new(role, content)
                .map_err(|e| LLMError::InvalidRequest(e.to_string()))?,
        );
    }

    if let Ok(template) = model.chat_template(cfg.chat_template.as_deref()) {
        if let Ok(prompt) = model.apply_chat_template(&template, &chat_messages, true) {
            return Ok((prompt, true));
        }
    }

    // Fall back to simple text concatenation
    let prompt = messages::messages_to_text(cfg, messages)?;
    Ok((prompt, false))
}

/// Build a prompt using the configured use_chat_template setting.
pub(crate) fn build_prompt(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
    media_marker: Option<&str>,
) -> Result<(String, bool), LLMError> {
    let use_chat_template = cfg.use_chat_template.unwrap_or(true);
    build_prompt_with(model, cfg, messages, use_chat_template, media_marker)
}

/// Build multiple prompt candidates for fallback.
pub(crate) fn build_prompt_candidates(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
    media_marker: Option<&str>,
) -> Result<Vec<String>, LLMError> {
    let (prompt, used_chat_template) = build_prompt(model, cfg, messages, media_marker)?;

    let mut prompts = vec![prompt];

    if used_chat_template && cfg.use_chat_template.is_none() {
        let (fallback_prompt, _) = build_prompt_with(model, cfg, messages, false, media_marker)?;
        if !prompts.contains(&fallback_prompt) {
            prompts.push(fallback_prompt);
        }
    }

    let raw_prompt = build_raw_prompt(cfg, messages)?;
    if !prompts.contains(&raw_prompt) {
        prompts.push(raw_prompt);
    }

    Ok(prompts)
}

/// Build a raw prompt without chat template.
pub(crate) fn build_raw_prompt(
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
) -> Result<String, LLMError> {
    // Use the unified messages_to_text function
    messages::messages_to_text(cfg, messages)
}

/// Generate text from a prompt, optionally with multimodal input.
///
/// When multimodal context and bitmaps are provided, uses MTMD tokenization
/// and evaluation. Otherwise falls back to standard text-only generation.
///
/// # Arguments
/// * `model` - The loaded LLM model
/// * `cfg` - Provider configuration
/// * `prompt` - Text prompt (may contain media markers if bitmaps provided)
/// * `max_tokens` - Maximum tokens to generate
/// * `temperature` - Sampling temperature (None for greedy)
/// * `mm_ctx` - Optional multimodal context (for vision/audio models)
/// * `bitmaps` - Image/audio bitmaps (must match marker count in prompt)
pub(crate) fn generate(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    prompt: &str,
    max_tokens: u32,
    temperature: Option<f32>,
    mm_ctx: Option<&MultimodalContext>,
    bitmaps: &[MtmdBitmap],
) -> Result<GeneratedText, LLMError> {
    let backend = llama_backend()?;

    // Validate: if bitmaps provided, must have mm_ctx
    if !bitmaps.is_empty() && mm_ctx.is_none() {
        return Err(LLMError::InvalidRequest(
            "Images provided but model does not support multimodal input. \
             Configure mmproj_path or use a vision-capable model."
                .into(),
        ));
    }

    let mut ctx_params = LlamaContextParams::default();
    let effective_n_ctx;
    let effective_n_batch;
    if let Some(n_ctx) = cfg.n_ctx {
        let n_ctx = NonZeroU32::new(n_ctx)
            .ok_or_else(|| LLMError::InvalidRequest("n_ctx must be greater than zero".into()))?;
        let n_batch = resolve_n_batch(cfg, n_ctx.get());
        let n_ubatch = resolve_n_ubatch(cfg, n_batch, mm_ctx.is_some());
        ctx_params = ctx_params.with_n_ctx(Some(n_ctx));
        ctx_params = ctx_params.with_n_batch(n_batch);
        ctx_params = ctx_params.with_n_ubatch(n_ubatch);
        effective_n_ctx = n_ctx.get();
        effective_n_batch = n_batch;
    } else {
        effective_n_ctx = 0; // will use llama.cpp default
        effective_n_batch = DEFAULT_N_BATCH_CAP;
    }
    if let Some(n_threads) = cfg.n_threads {
        ctx_params = ctx_params.with_n_threads(n_threads);
    }
    if let Some(n_threads_batch) = cfg.n_threads_batch {
        ctx_params = ctx_params.with_n_threads_batch(n_threads_batch);
    }
    ctx_params = apply_context_params(cfg, ctx_params)?;

    let mut ctx = model.new_context(&*backend, ctx_params).map_err(|e| {
        let n = if effective_n_ctx > 0 {
            effective_n_ctx
        } else {
            512
        };
        let est = estimate_context_memory(model, cfg, n);
        LLMError::ProviderError(format!(
            "Failed to create context: {}. {}\n\
             Try reducing n_ctx or using KV cache quantization.",
            e,
            est.summary()
        ))
    })?;

    let n_ctx_total = ctx.n_ctx() as i32;
    let n_batch = resolve_n_batch(cfg, n_ctx_total as u32);

    // UNIFIED TOKENIZATION AND EVALUATION
    let (n_past, input_tokens) = if let Some(mm_ctx) = mm_ctx {
        // Multimodal path: use MTMD tokenization
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

        let total_tokens = chunks.total_tokens();

        // Validate that each media chunk fits within n_ubatch.
        // Vision models (Qwen-VL, LLaVA, etc.) use non-causal attention for
        // image decoding, requiring all image tokens in a single ubatch.
        let n_ubatch = resolve_n_ubatch(cfg, effective_n_batch, true) as usize;
        for i in 0..chunks.len() {
            if let Some(chunk) = chunks.get(i) {
                if chunk.chunk_type() != MtmdInputChunkType::Text {
                    let img_tokens = chunk.n_tokens();
                    if img_tokens > n_ubatch {
                        return Err(LLMError::InvalidRequest(format!(
                            "Image produces {img_tokens} tokens but n_ubatch is {n_ubatch}. \
                             Increase n_batch/n_ubatch or use a lower-resolution image."
                        )));
                    }
                }
            }
        }

        // Early exit for max_tokens=0
        if max_tokens == 0 {
            return Ok(GeneratedText {
                text: String::new(),
                usage: Usage {
                    input_tokens: total_tokens as u32,
                    output_tokens: 0,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning_tokens: 0,
                },
            });
        }

        // Check if we fit in context
        let n_len_total = total_tokens as i32 + max_tokens as i32;
        if n_len_total > n_ctx_total {
            return Err(LLMError::InvalidRequest(format!(
                "Prompt + max_tokens ({}) exceeds context window ({})",
                n_len_total, n_ctx_total
            )));
        }

        // Evaluate chunks (handles both text and image encoding)
        let n_past = chunks
            .eval_chunks(
                &mm_ctx.ctx,
                &mut ctx,
                0, // n_past starts at 0
                0, // seq_id
                n_batch as i32,
                true, // logits_last
            )
            .map_err(|e| LLMError::ProviderError(format!("MTMD evaluation failed: {}", e)))?;

        (n_past, total_tokens)
    } else {
        // Text-only path: standard tokenization
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

        let input_tokens = tokens.len();

        // Early exit for max_tokens=0
        if max_tokens == 0 {
            return Ok(GeneratedText {
                text: String::new(),
                usage: Usage {
                    input_tokens: input_tokens as u32,
                    output_tokens: 0,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning_tokens: 0,
                },
            });
        }

        // Check if we fit in context
        let n_len_total = tokens.len() as i32 + max_tokens as i32;
        if n_len_total > n_ctx_total {
            return Err(LLMError::InvalidRequest(format!(
                "Prompt + max_tokens ({}) exceeds context window ({})",
                n_len_total, n_ctx_total
            )));
        }

        // Decode prompt in chunks (standard batched decode)
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
                let est = estimate_context_memory(model, cfg, n_ctx_total as u32);
                LLMError::ProviderError(format!(
                    "Failed to decode prompt batch (n_ctx={}): {}. {}",
                    n_ctx_total,
                    e,
                    est.summary()
                ))
            })?;
        }

        (tokens.len() as i32, input_tokens)
    };

    // UNIFIED GENERATION PHASE (identical for both paths)

    let seed = cfg.seed.unwrap_or(1234);
    let mut sampler = build_standard_sampler(temperature, seed, cfg.top_p, cfg.top_k, cfg.min_p);
    let allow_fallback = temperature.is_none()
        && cfg.temperature.is_none()
        && cfg.top_p.is_none()
        && cfg.top_k.is_none()
        && cfg.min_p.is_none();
    let mut fallback_used = false;

    let mut n_cur = n_past;
    let n_len_total = n_cur + max_tokens as i32;
    let mut batch = LlamaBatch::new(n_batch as usize, 1);
    let mut output_tokens = 0u32;
    let mut output = String::new();
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    while n_cur < n_len_total {
        let token = sampler.sample(&ctx, batch.n_tokens() - 1);
        if model.is_eog_token(token) {
            if output_tokens == 0 && allow_fallback && !fallback_used {
                sampler = build_fallback_sampler(seed);
                fallback_used = true;
                continue;
            }
            break;
        }
        sampler.accept(token);

        let bytes = model
            .token_to_piece_bytes(token, 128, true, None)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        let chunk = match model.token_to_piece(token, &mut decoder, true, None) {
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
            input_tokens: input_tokens as u32,
            output_tokens,
            cache_read: 0,
            cache_write: 0,
            reasoning_tokens: 0,
        },
    })
}

/// Generate text with streaming.
pub(crate) fn generate_streaming(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    prompt: &str,
    max_tokens: u32,
    temperature: Option<f32>,
    tx: &mpsc::UnboundedSender<Result<querymt::chat::StreamChunk, LLMError>>,
    mm_ctx: Option<&MultimodalContext>,
    bitmaps: &[MtmdBitmap],
) -> Result<Usage, LLMError> {
    let backend = llama_backend()?;

    // Validate: if bitmaps provided, must have mm_ctx
    if !bitmaps.is_empty() && mm_ctx.is_none() {
        return Err(LLMError::InvalidRequest(
            "Images provided but model does not support multimodal input. \
             Configure mmproj_path or use a vision-capable model."
                .into(),
        ));
    }

    // Setup context parameters (same for both paths)
    let mut ctx_params = LlamaContextParams::default();
    let effective_n_ctx;
    let effective_n_batch;
    if let Some(n_ctx) = cfg.n_ctx {
        let n_ctx = NonZeroU32::new(n_ctx)
            .ok_or_else(|| LLMError::InvalidRequest("n_ctx must be greater than zero".into()))?;
        let n_batch = resolve_n_batch(cfg, n_ctx.get());
        let n_ubatch = resolve_n_ubatch(cfg, n_batch, mm_ctx.is_some());
        ctx_params = ctx_params.with_n_ctx(Some(n_ctx));
        ctx_params = ctx_params.with_n_batch(n_batch);
        ctx_params = ctx_params.with_n_ubatch(n_ubatch);
        effective_n_ctx = n_ctx.get();
        effective_n_batch = n_batch;
    } else {
        effective_n_ctx = 0; // use llama.cpp default
        effective_n_batch = DEFAULT_N_BATCH_CAP;
    };

    if let Some(n_threads) = cfg.n_threads {
        ctx_params = ctx_params.with_n_threads(n_threads);
    }
    if let Some(n_threads_batch) = cfg.n_threads_batch {
        ctx_params = ctx_params.with_n_threads_batch(n_threads_batch);
    }
    ctx_params = apply_context_params(cfg, ctx_params)?;

    let mut ctx = model.new_context(&*backend, ctx_params).map_err(|e| {
        let n = if effective_n_ctx > 0 {
            effective_n_ctx
        } else {
            512
        };
        let est = estimate_context_memory(model, cfg, n);
        LLMError::ProviderError(format!(
            "Failed to create context: {}. {}\n\
             Try reducing n_ctx or using KV cache quantization.",
            e,
            est.summary()
        ))
    })?;

    let n_ctx_total = ctx.n_ctx() as i32;
    let n_batch = resolve_n_batch(cfg, n_ctx_total as u32);

    // UNIFIED TOKENIZATION AND EVALUATION
    let (n_past, input_tokens) = if let Some(mm_ctx) = mm_ctx {
        // Multimodal path: use MTMD tokenization
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

        let total_tokens = chunks.total_tokens();

        // Validate that each media chunk fits within n_ubatch.
        // Vision models (Qwen-VL, LLaVA, etc.) use non-causal attention for
        // image decoding, requiring all image tokens in a single ubatch.
        let n_ubatch = resolve_n_ubatch(cfg, effective_n_batch, true) as usize;
        for i in 0..chunks.len() {
            if let Some(chunk) = chunks.get(i) {
                if chunk.chunk_type() != MtmdInputChunkType::Text {
                    let img_tokens = chunk.n_tokens();
                    if img_tokens > n_ubatch {
                        return Err(LLMError::InvalidRequest(format!(
                            "Image produces {img_tokens} tokens but n_ubatch is {n_ubatch}. \
                             Increase n_batch/n_ubatch or use a lower-resolution image."
                        )));
                    }
                }
            }
        }

        // Early exit for max_tokens=0
        if max_tokens == 0 {
            return Ok(Usage {
                input_tokens: total_tokens as u32,
                output_tokens: 0,
                cache_read: 0,
                cache_write: 0,
                reasoning_tokens: 0,
            });
        }

        // Check if we fit in context
        let n_len_total = total_tokens as i32 + max_tokens as i32;
        if n_len_total > n_ctx_total {
            return Err(LLMError::InvalidRequest(format!(
                "Prompt + max_tokens ({}) exceeds context window ({})",
                n_len_total, n_ctx_total
            )));
        }

        // Evaluate chunks (handles both text and image encoding)
        let n_past = chunks
            .eval_chunks(
                &mm_ctx.ctx,
                &mut ctx,
                0, // n_past starts at 0
                0, // seq_id
                n_batch as i32,
                true, // logits_last
            )
            .map_err(|e| LLMError::ProviderError(format!("MTMD evaluation failed: {}", e)))?;

        (n_past, total_tokens)
    } else {
        // Text-only path: standard tokenization
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

        let input_tokens = tokens.len();

        // Early exit for max_tokens=0
        if max_tokens == 0 {
            return Ok(Usage {
                input_tokens: input_tokens as u32,
                output_tokens: 0,
                cache_read: 0,
                cache_write: 0,
                reasoning_tokens: 0,
            });
        }

        // Check if we fit in context
        let n_len_total = tokens.len() as i32 + max_tokens as i32;
        if n_len_total > n_ctx_total {
            return Err(LLMError::InvalidRequest(format!(
                "Prompt + max_tokens ({}) exceeds context window ({})",
                n_len_total, n_ctx_total
            )));
        }

        // Decode prompt in chunks (standard batched decode)
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
                let est = estimate_context_memory(model, cfg, n_ctx_total as u32);
                LLMError::ProviderError(format!(
                    "Failed to decode prompt batch (n_ctx={}): {}. {}",
                    n_ctx_total,
                    e,
                    est.summary()
                ))
            })?;
        }

        (tokens.len() as i32, input_tokens)
    };

    // UNIFIED GENERATION PHASE (identical for both paths)
    let seed = cfg.seed.unwrap_or(1234);
    let mut sampler = build_standard_sampler(temperature, seed, cfg.top_p, cfg.top_k, cfg.min_p);
    let allow_fallback = temperature.is_none()
        && cfg.temperature.is_none()
        && cfg.top_p.is_none()
        && cfg.top_k.is_none()
        && cfg.min_p.is_none();
    let mut fallback_used = false;

    let mut n_cur = n_past;
    let n_len_total = n_cur + max_tokens as i32;
    let mut batch = LlamaBatch::new(n_batch as usize, 1);
    let mut output_tokens = 0u32;
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    while n_cur < n_len_total {
        let token = sampler.sample(&ctx, batch.n_tokens() - 1);
        if model.is_eog_token(token) {
            if output_tokens == 0 && allow_fallback && !fallback_used {
                sampler = build_fallback_sampler(seed);
                fallback_used = true;
                continue;
            }
            break;
        }
        sampler.accept(token);

        let bytes = model
            .token_to_piece_bytes(token, 128, true, None)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        let chunk = match model.token_to_piece(token, &mut decoder, true, None) {
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
        input_tokens: input_tokens as u32,
        output_tokens,
        cache_read: 0,
        cache_write: 0,
        reasoning_tokens: 0,
    })
}

/// Generate text with streaming, routing thinking tokens to `StreamChunk::Thinking`.
///
/// Uses the OAI-compat streaming state machine from `result.streaming_state_oaicompat()`
/// so that `reasoning_content` deltas from the model's `<think>` block are emitted as
/// [`querymt::chat::StreamChunk::Thinking`] and regular `content` deltas are emitted as
/// [`querymt::chat::StreamChunk::Text`].  No grammar or tool-call handling is performed.
///
/// When `mm_ctx` and `bitmaps` are provided the function uses MTMD tokenization and
/// evaluation so that image data is encoded into the KV-cache before generation begins.
/// The prompt in `result` must already contain the media marker tokens at the correct
/// positions (injected by `messages_to_json` → `apply_template_for_thinking`).
pub(crate) fn generate_streaming_with_thinking(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    result: &ChatTemplateResult,
    max_tokens: u32,
    temperature: Option<f32>,
    tx: &mpsc::UnboundedSender<Result<querymt::chat::StreamChunk, LLMError>>,
    mm_ctx: Option<&MultimodalContext>,
    bitmaps: &[MtmdBitmap],
) -> Result<Usage, LLMError> {
    let backend = llama_backend()?;

    // Validate: bitmaps require a multimodal context.
    if !bitmaps.is_empty() && mm_ctx.is_none() {
        return Err(LLMError::InvalidRequest(
            "Images provided but model does not support multimodal input. \
             Configure mmproj_path or use a vision-capable model."
                .into(),
        ));
    }

    let mut ctx_params = LlamaContextParams::default();
    let effective_n_ctx;
    let effective_n_batch;
    if let Some(n_ctx) = cfg.n_ctx {
        let n_ctx = NonZeroU32::new(n_ctx)
            .ok_or_else(|| LLMError::InvalidRequest("n_ctx must be greater than zero".into()))?;
        let n_batch = resolve_n_batch(cfg, n_ctx.get());
        let n_ubatch = resolve_n_ubatch(cfg, n_batch, mm_ctx.is_some());
        ctx_params = ctx_params.with_n_ctx(Some(n_ctx));
        ctx_params = ctx_params.with_n_batch(n_batch);
        ctx_params = ctx_params.with_n_ubatch(n_ubatch);
        effective_n_ctx = n_ctx.get();
        effective_n_batch = n_batch;
    } else {
        effective_n_ctx = 0; // will use llama.cpp default
        effective_n_batch = DEFAULT_N_BATCH_CAP;
    }
    if let Some(n_threads) = cfg.n_threads {
        ctx_params = ctx_params.with_n_threads(n_threads);
    }
    if let Some(n_threads_batch) = cfg.n_threads_batch {
        ctx_params = ctx_params.with_n_threads_batch(n_threads_batch);
    }
    ctx_params = apply_context_params(cfg, ctx_params)?;

    let mut ctx = model.new_context(&*backend, ctx_params).map_err(|e| {
        let n = if effective_n_ctx > 0 {
            effective_n_ctx
        } else {
            512
        };
        let est = estimate_context_memory(model, cfg, n);
        LLMError::ProviderError(format!(
            "Failed to create context: {}. {}\n\
                     Try reducing n_ctx or using KV cache quantization.",
            e,
            est.summary()
        ))
    })?;

    let n_ctx_total = ctx.n_ctx() as i32;
    let n_batch = resolve_n_batch(cfg, n_ctx_total as u32) as usize;
    let mut batch = LlamaBatch::new(n_batch, 1);

    // TOKENIZATION AND EVALUATION — dual path: multimodal vs text-only
    let (n_past, input_tokens) = if let Some(mm_ctx) = mm_ctx {
        // Multimodal path: use MTMD tokenization so image embeddings are encoded.
        let input_text = MtmdInputText {
            text: result.prompt.clone(),
            add_special: cfg.add_bos.unwrap_or(true),
            parse_special: true,
        };

        let bitmap_refs: Vec<&MtmdBitmap> = bitmaps.iter().collect();
        log::debug!(
            "generate_streaming_with_thinking: MTMD tokenization with {} bitmap(s)",
            bitmap_refs.len()
        );
        let chunks = mm_ctx
            .ctx
            .tokenize(input_text, &bitmap_refs)
            .map_err(|e| LLMError::ProviderError(format!("MTMD tokenization failed: {}", e)))?;

        let total_tokens = chunks.total_tokens();

        // Validate that each media chunk fits within n_ubatch.
        // Vision models (Qwen-VL, LLaVA, etc.) use non-causal attention for
        // image decoding, requiring all image tokens in a single ubatch.
        let n_ubatch = resolve_n_ubatch(cfg, effective_n_batch, true) as usize;
        for i in 0..chunks.len() {
            if let Some(chunk) = chunks.get(i) {
                if chunk.chunk_type() != MtmdInputChunkType::Text {
                    let img_tokens = chunk.n_tokens();
                    if img_tokens > n_ubatch {
                        return Err(LLMError::InvalidRequest(format!(
                            "Image produces {img_tokens} tokens but n_ubatch is {n_ubatch}. \
                             Increase n_batch/n_ubatch or use a lower-resolution image."
                        )));
                    }
                }
            }
        }

        if max_tokens == 0 {
            return Ok(Usage {
                input_tokens: total_tokens as u32,
                output_tokens: 0,
                cache_read: 0,
                cache_write: 0,
                reasoning_tokens: 0,
            });
        }

        let n_len_total = total_tokens as i32 + max_tokens as i32;
        if n_len_total > n_ctx_total {
            return Err(LLMError::InvalidRequest(format!(
                "Prompt + max_tokens ({n_len_total}) exceeds context window ({n_ctx_total})"
            )));
        }

        let n_past = chunks
            .eval_chunks(
                &mm_ctx.ctx,
                &mut ctx,
                0, // n_past starts at 0
                0, // seq_id
                n_batch as i32,
                true, // logits_last
            )
            .map_err(|e| LLMError::ProviderError(format!("MTMD evaluation failed: {}", e)))?;

        (n_past, total_tokens)
    } else {
        // Text-only path: standard tokenization.
        let add_bos = cfg.add_bos.unwrap_or(true);
        let tokens = model
            .str_to_token(
                &result.prompt,
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

        let n_len_total = tokens.len() as i32 + max_tokens as i32;
        if n_len_total > n_ctx_total {
            return Err(LLMError::InvalidRequest(format!(
                "Prompt + max_tokens ({n_len_total}) exceeds context window ({n_ctx_total})"
            )));
        }

        // Decode prompt in chunks of n_batch.
        let last_index = tokens.len().saturating_sub(1);
        for chunk_start in (0..tokens.len()).step_by(n_batch) {
            batch.clear();
            let chunk_end = (chunk_start + n_batch).min(tokens.len());
            for i in chunk_start..chunk_end {
                let is_last = i == last_index;
                batch
                    .add(tokens[i], i as i32, &[0], is_last)
                    .map_err(|e| LLMError::ProviderError(e.to_string()))?;
            }
            ctx.decode(&mut batch).map_err(|e| {
                let est = estimate_context_memory(model, cfg, n_ctx_total as u32);
                LLMError::ProviderError(format!(
                    "Failed to decode prompt batch (n_ctx={}): {}. {}",
                    n_ctx_total,
                    e,
                    est.summary()
                ))
            })?;
        }

        (tokens.len() as i32, tokens.len())
    };

    // Initialise the OAI-compat streaming state machine.  It will parse
    // `reasoning_content` vs `content` fields from llama.cpp's internal JSON
    // deltas and return them as separate delta strings.
    let mut stream_state = result
        .streaming_state_oaicompat()
        .map_err(|e| LLMError::ProviderError(format!("Failed to init streaming state: {}", e)))?;

    let seed = cfg.seed.unwrap_or(1234);
    let mut sampler = build_standard_sampler(temperature, seed, cfg.top_p, cfg.top_k, cfg.min_p);
    let allow_fallback = temperature.is_none()
        && cfg.temperature.is_none()
        && cfg.top_p.is_none()
        && cfg.top_k.is_none()
        && cfg.min_p.is_none();
    let mut fallback_used = false;

    let mut n_cur = n_past;
    let n_len_total = n_past + max_tokens as i32;
    let mut output_tokens = 0u32;
    let mut decoder = encoding_rs::UTF_8.new_decoder();

    while n_cur < n_len_total {
        let token = sampler.sample(&ctx, batch.n_tokens() - 1);
        if model.is_eog_token(token) {
            if output_tokens == 0 && allow_fallback && !fallback_used {
                sampler = build_fallback_sampler(seed);
                fallback_used = true;
                continue;
            }
            break;
        }
        sampler.accept(token);

        let bytes = model
            .token_to_piece_bytes(token, 128, true, None)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        let chunk = match model.token_to_piece(token, &mut decoder, true, None) {
            Ok(piece) => piece,
            Err(_) => String::from_utf8_lossy(&bytes).to_string(),
        };

        // Feed the token piece into the OAI-compat streaming parser.
        // `continue` = true because we have not hit a stop sequence.
        match stream_state.update(&chunk, true) {
            Ok(deltas) => {
                for delta_json in deltas {
                    if let Ok(delta) = serde_json::from_str::<serde_json::Value>(&delta_json) {
                        // Regular content → Text
                        if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                            if !content.is_empty()
                                && tx
                                    .unbounded_send(Ok(querymt::chat::StreamChunk::Text(
                                        content.to_string(),
                                    )))
                                    .is_err()
                            {
                                return Ok(Usage {
                                    input_tokens: input_tokens as u32,
                                    output_tokens,
                                    cache_read: 0,
                                    cache_write: 0,
                                    reasoning_tokens: 0,
                                });
                            }
                        }

                        // Reasoning content → Thinking
                        if let Some(reasoning) =
                            delta.get("reasoning_content").and_then(|v| v.as_str())
                        {
                            if !reasoning.is_empty()
                                && tx
                                    .unbounded_send(Ok(querymt::chat::StreamChunk::Thinking(
                                        reasoning.to_string(),
                                    )))
                                    .is_err()
                            {
                                return Ok(Usage {
                                    input_tokens: input_tokens as u32,
                                    output_tokens,
                                    cache_read: 0,
                                    cache_write: 0,
                                    reasoning_tokens: 0,
                                });
                            }
                        }
                    }
                }
            }
            Err(e) => {
                let _ = tx.unbounded_send(Err(LLMError::ProviderError(format!(
                    "Streaming parse error: {}",
                    e
                ))));
                return Err(LLMError::ProviderError(format!(
                    "Streaming parse error: {}",
                    e
                )));
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
        input_tokens: input_tokens as u32,
        output_tokens,
        cache_read: 0,
        cache_write: 0,
        reasoning_tokens: 0,
    })
}
