use crate::backend::llama_backend;
use crate::config::LlamaCppConfig;
use crate::context::{apply_context_params, estimate_context_memory, resolve_n_batch};
use crate::response::GeneratedText;
use crate::tools::sampler::{build_fallback_sampler, build_standard_sampler};
use futures::channel::mpsc;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaModel};
use querymt::Usage;
use querymt::chat::{ChatMessage, ChatRole, MessageType};
use querymt::error::LLMError;
use std::num::NonZeroU32;
use std::sync::Arc;

/// Build a prompt from chat messages using optional chat template.
pub(crate) fn build_prompt_with(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
    use_chat_template: bool,
) -> Result<(String, bool), LLMError> {
    for msg in messages {
        if !matches!(msg.message_type, MessageType::Text) {
            return Err(LLMError::InvalidRequest(
                "Only text chat messages are supported by llama.cpp provider".into(),
            ));
        }
    }

    if !use_chat_template {
        let prompt = build_raw_prompt(cfg, messages)?;
        return Ok((prompt, false));
    }

    let mut chat_messages = Vec::with_capacity(messages.len() + 1);
    if !cfg.system.is_empty() {
        let system = cfg.system.join("\n\n");
        chat_messages.push(
            LlamaChatMessage::new("system".to_string(), system)
                .map_err(|e| LLMError::InvalidRequest(e.to_string()))?,
        );
    }

    for msg in messages {
        let role = match msg.role {
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
        };
        chat_messages.push(
            LlamaChatMessage::new(role.to_string(), msg.content.clone())
                .map_err(|e| LLMError::InvalidRequest(e.to_string()))?,
        );
    }

    if let Ok(template) = model.chat_template(cfg.chat_template.as_deref()) {
        if let Ok(prompt) = model.apply_chat_template(&template, &chat_messages, true) {
            return Ok((prompt, true));
        }
    }

    let prompt = build_raw_prompt(cfg, messages)?;
    Ok((prompt, false))
}

/// Build a prompt using the configured use_chat_template setting.
pub(crate) fn build_prompt(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
) -> Result<(String, bool), LLMError> {
    let use_chat_template = cfg.use_chat_template.unwrap_or(true);
    build_prompt_with(model, cfg, messages, use_chat_template)
}

/// Build multiple prompt candidates for fallback.
pub(crate) fn build_prompt_candidates(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
) -> Result<Vec<String>, LLMError> {
    let (prompt, used_chat_template) = build_prompt(model, cfg, messages)?;
    let mut prompts = vec![prompt];

    if used_chat_template && cfg.use_chat_template.is_none() {
        let (fallback_prompt, _) = build_prompt_with(model, cfg, messages, false)?;
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
    for msg in messages {
        if !matches!(msg.message_type, MessageType::Text) {
            return Err(LLMError::InvalidRequest(
                "Only text chat messages are supported by llama.cpp provider".into(),
            ));
        }
    }

    let mut prompt = String::new();
    if !cfg.system.is_empty() {
        prompt.push_str(&cfg.system.join("\n\n"));
        prompt.push_str("\n\n");
    }
    for (idx, msg) in messages.iter().enumerate() {
        prompt.push_str(&msg.content);
        if idx + 1 < messages.len() {
            prompt.push_str("\n\n");
        }
    }
    Ok(prompt)
}

/// Generate text from a prompt.
pub(crate) fn generate(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    prompt: &str,
    max_tokens: u32,
    temperature: Option<f32>,
) -> Result<GeneratedText, LLMError> {
    let backend = llama_backend()?;
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
    if max_tokens == 0 {
        return Ok(GeneratedText {
            text: String::new(),
            usage: Usage {
                input_tokens: tokens.len() as u32,
                output_tokens: 0,
                cache_read: 0,
                cache_write: 0,
                reasoning_tokens: 0,
            },
        });
    }

    let mut ctx_params = LlamaContextParams::default();
    let effective_n_ctx;
    if let Some(n_ctx) = cfg.n_ctx {
        let n_ctx = NonZeroU32::new(n_ctx)
            .ok_or_else(|| LLMError::InvalidRequest("n_ctx must be greater than zero".into()))?;
        ctx_params = ctx_params.with_n_ctx(Some(n_ctx));
        ctx_params = ctx_params.with_n_batch(resolve_n_batch(cfg, n_ctx.get()));
        effective_n_ctx = n_ctx.get();
    } else {
        effective_n_ctx = 0; // will use llama.cpp default
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

    let n_len_total = tokens.len() as i32 + max_tokens as i32;
    if n_len_total > n_ctx_total {
        return Err(LLMError::InvalidRequest(format!(
            "Prompt + max_tokens ({n_len_total}) exceeds context window ({n_ctx_total})"
        )));
    }

    let n_batch = resolve_n_batch(cfg, n_ctx_total as u32) as usize;
    let mut batch = LlamaBatch::new(n_batch, 1);

    // Decode prompt in chunks of n_batch
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

    let seed = cfg.seed.unwrap_or(1234);
    let mut sampler = build_standard_sampler(temperature, seed, cfg.top_p, cfg.top_k);
    let allow_fallback = temperature.is_none()
        && cfg.temperature.is_none()
        && cfg.top_p.is_none()
        && cfg.top_k.is_none();
    let mut fallback_used = false;

    let mut n_cur = tokens.len() as i32;
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
            input_tokens: tokens.len() as u32,
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
) -> Result<Usage, LLMError> {
    let backend = llama_backend()?;
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
    if max_tokens == 0 {
        return Ok(Usage {
            input_tokens: tokens.len() as u32,
            output_tokens: 0,
            cache_read: 0,
            cache_write: 0,
            reasoning_tokens: 0,
        });
    }

    let mut ctx_params = LlamaContextParams::default();
    let effective_n_ctx;
    if let Some(n_ctx) = cfg.n_ctx {
        let n_ctx = NonZeroU32::new(n_ctx)
            .ok_or_else(|| LLMError::InvalidRequest("n_ctx must be greater than zero".into()))?;
        ctx_params = ctx_params.with_n_ctx(Some(n_ctx));
        ctx_params = ctx_params.with_n_batch(resolve_n_batch(cfg, n_ctx.get()));
        effective_n_ctx = n_ctx.get();
    } else {
        effective_n_ctx = 0; // will use llama.cpp default
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
    let n_len_total = tokens.len() as i32 + max_tokens as i32;
    if n_len_total > n_ctx_total {
        return Err(LLMError::InvalidRequest(format!(
            "Prompt + max_tokens ({n_len_total}) exceeds context window ({n_ctx_total})"
        )));
    }

    let n_batch = resolve_n_batch(cfg, n_ctx_total as u32) as usize;
    let mut batch = LlamaBatch::new(n_batch, 1);

    // Decode prompt in chunks of n_batch
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

    let seed = cfg.seed.unwrap_or(1234);
    let mut sampler = build_standard_sampler(temperature, seed, cfg.top_p, cfg.top_k);
    let allow_fallback = temperature.is_none()
        && cfg.temperature.is_none()
        && cfg.top_p.is_none()
        && cfg.top_k.is_none();
    let mut fallback_used = false;

    let mut n_cur = tokens.len() as i32;
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
        input_tokens: tokens.len() as u32,
        output_tokens,
        cache_read: 0,
        cache_write: 0,
        reasoning_tokens: 0,
    })
}
