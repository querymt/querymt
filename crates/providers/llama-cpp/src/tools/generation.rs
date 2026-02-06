use crate::backend::llama_backend;
use crate::config::LlamaCppConfig;
use crate::context::{apply_context_params, estimate_context_memory, resolve_n_batch};
use crate::response::GeneratedText;
use crate::tools::sampler::build_tool_sampler;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::{AddBos, ChatTemplateResult, LlamaModel};
use querymt::error::LLMError;
use querymt::Usage;
use serde_json::Value;
use std::collections::HashSet;
use std::num::NonZeroU32;
use std::sync::Arc;

/// Generate text with grammar-constrained sampling for tool calls.
pub(crate) fn generate_with_tools(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    result: &ChatTemplateResult,
    max_tokens: u32,
    temperature: Option<f32>,
) -> Result<GeneratedText, LLMError> {
    let backend = llama_backend()?;
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

    log::debug!(
        "Generating with tools: input_tokens={}, max_tokens={}, add_bos={}",
        tokens.len(),
        max_tokens,
        add_bos
    );

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

    // Auto-size context when not explicitly configured (tool prompts are large)
    let n_ctx_needed = tokens.len() as u32 + max_tokens;
    let n_ctx = if let Some(configured_n_ctx) = cfg.n_ctx {
        configured_n_ctx
    } else {
        // Only allocate what we actually need; cap at the model's training
        // context to avoid GPU out-of-memory when n_ctx is not configured.
        n_ctx_needed.min(model.n_ctx_train())
    };

    log::debug!(
        "Context sizing: needed={}, configured={:?}, model_train={}, using={}",
        n_ctx_needed,
        cfg.n_ctx,
        model.n_ctx_train(),
        n_ctx
    );

    let mut ctx_params = LlamaContextParams::default();
    let n_ctx = NonZeroU32::new(n_ctx)
        .ok_or_else(|| LLMError::InvalidRequest("n_ctx must be greater than zero".into()))?;
    ctx_params = ctx_params.with_n_ctx(Some(n_ctx));
    ctx_params = ctx_params.with_n_batch(resolve_n_batch(cfg, n_ctx.get()));
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

    let n_batch = resolve_n_batch(cfg, n_ctx.get()) as usize;
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
            let est = estimate_context_memory(model, cfg, n_ctx.get());
            LLMError::ProviderError(format!(
                "Failed to decode prompt batch (chunk {}/{}, n_ctx={}): {}. {}",
                chunk_start / n_batch + 1,
                (tokens.len() + n_batch - 1) / n_batch,
                n_ctx.get(),
                e,
                est.summary()
            ))
        })?;
    }

    // Build preserved token set for special handling
    let mut preserved = HashSet::new();
    for token_str in &result.preserved_tokens {
        if let Ok(preserved_tokens) = model.str_to_token(token_str, AddBos::Never) {
            if preserved_tokens.len() == 1 {
                preserved.insert(preserved_tokens[0]);
            }
        }
    }

    let seed = cfg.seed.unwrap_or(1234);
    let mut sampler = build_tool_sampler(model, result, temperature, seed, cfg.top_p, cfg.top_k);
    let mut n_cur = tokens.len() as i32;
    let mut output_tokens = 0u32;
    let mut output = String::new();
    let mut decoder = encoding_rs::UTF_8.new_decoder();

    while n_cur < n_len_total {
        let token = sampler.sample(&ctx, batch.n_tokens() - 1);
        if model.is_eog_token(token) {
            break;
        }

        let special = preserved.contains(&token);
        let bytes = model
            .token_to_piece_bytes(token, 128, special, None)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        let chunk = match model.token_to_piece(token, &mut decoder, special, None) {
            Ok(piece) => piece,
            Err(_) => String::from_utf8_lossy(&bytes).to_string(),
        };
        output.push_str(&chunk);

        // Check additional stop sequences
        if result
            .additional_stops
            .iter()
            .any(|stop| !stop.is_empty() && output.ends_with(stop))
        {
            break;
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

    // Trim matched stop sequences
    for stop in &result.additional_stops {
        if !stop.is_empty() && output.ends_with(stop) {
            let new_len = output.len().saturating_sub(stop.len());
            output.truncate(new_len);
            break;
        }
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

/// Parse the generated response using the ChatTemplateResult to extract tool calls.
///
/// Returns (content, thinking, tool_calls, finish_reason).
pub(crate) fn parse_tool_response(
    result: &ChatTemplateResult,
    text: &str,
) -> Result<
    (
        String,
        Option<String>,
        Option<Vec<querymt::ToolCall>>,
        querymt::chat::FinishReason,
    ),
    LLMError,
> {
    log::debug!("Parsing tool response: text_len={}", text.len());
    log::debug!("Raw generated text: {}", text);

    let parsed_json = result.parse_response_oaicompat(text, false).map_err(|e| {
        log::debug!(
            "Failed to parse response with parse_response_oaicompat: {}",
            e
        );
        LLMError::ProviderError(format!("Failed to parse response: {}", e))
    })?;

    log::debug!("Parsed JSON: {}", parsed_json);

    let parsed: Value = serde_json::from_str(&parsed_json).map_err(|e| {
        LLMError::ProviderError(format!("Failed to deserialize parsed response: {}", e))
    })?;

    let raw_content = parsed
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // The C++ parser may have already extracted reasoning_content.
    // If so, use it directly. Otherwise, use extract_thinking as fallback.
    let reasoning_content = parsed
        .get("reasoning_content")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());

    let (thinking, content) = if reasoning_content.is_some() {
        (reasoning_content, raw_content)
    } else {
        querymt::chat::extract_thinking(&raw_content)
    };

    let tool_calls = parsed
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            if arr.is_empty() {
                None
            } else {
                serde_json::from_value(Value::Array(arr.clone())).ok()
            }
        });

    let finish_reason = if tool_calls.is_some() {
        querymt::chat::FinishReason::ToolCalls
    } else {
        querymt::chat::FinishReason::Stop
    };

    log::debug!(
        "Parsed response: content_len={}, thinking={}, tool_calls={}, finish_reason={:?}",
        content.len(),
        thinking.as_ref().map(|t| t.len()).unwrap_or(0),
        tool_calls
            .as_ref()
            .map(|tc: &Vec<querymt::ToolCall>| tc.len())
            .unwrap_or(0),
        finish_reason
    );

    Ok((content, thinking, tool_calls, finish_reason))
}
