use crate::backend::llama_backend;
use crate::config::LlamaCppConfig;
use crate::context::{apply_context_params, estimate_context_memory, resolve_n_batch};
use crate::tools::generation::parse_tool_response;
use crate::tools::sampler::build_tool_sampler;
use futures::channel::mpsc;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::{AddBos, ChatTemplateResult, LlamaModel};
use querymt::Usage;
use querymt::error::LLMError;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::sync::Arc;

/// Generate text with streaming and grammar-constrained sampling for tool calls.
/// Returns (Usage, has_tool_calls) where has_tool_calls indicates if tool calls were made.
pub(crate) fn generate_streaming_with_tools(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    result: &ChatTemplateResult,
    max_tokens: u32,
    temperature: Option<f32>,
    tx: &mpsc::UnboundedSender<Result<querymt::chat::StreamChunk, LLMError>>,
) -> Result<(Usage, bool), LLMError> {
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
        "Streaming generation with tools: input_tokens={}, max_tokens={}",
        tokens.len(),
        max_tokens
    );

    if max_tokens == 0 {
        return Ok((
            Usage {
                input_tokens: tokens.len() as u32,
                output_tokens: 0,
                cache_read: 0,
                cache_write: 0,
                reasoning_tokens: 0,
            },
            false,
        ));
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

    // Initialize streaming parser
    let mut stream_state = result
        .streaming_state_oaicompat()
        .map_err(|e| LLMError::ProviderError(format!("Failed to init streaming state: {}", e)))?;

    let seed = cfg.seed.unwrap_or(1234);
    let mut sampler = build_tool_sampler(model, result, temperature, seed, cfg.top_p, cfg.top_k, cfg.min_p);
    let mut n_cur = tokens.len() as i32;
    let mut output_tokens = 0u32;
    let mut generated_text = String::new();
    let mut decoder = encoding_rs::UTF_8.new_decoder();

    // Track tool calls being assembled
    let mut tool_calls_in_progress: HashMap<usize, (String, String, String)> = HashMap::new();

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
        generated_text.push_str(&chunk);

        // Check additional stop sequences
        let stop_now = result
            .additional_stops
            .iter()
            .any(|stop| !stop.is_empty() && generated_text.ends_with(stop));

        // Update streaming parser
        match stream_state.update(&chunk, !stop_now) {
            Ok(deltas) => {
                for delta_json in deltas {
                    // Parse each delta and emit appropriate StreamChunks
                    if let Ok(delta) = serde_json::from_str::<Value>(&delta_json) {
                        // Handle content delta
                        if let Some(content_delta) = delta.get("content").and_then(|v| v.as_str()) {
                            if !content_delta.is_empty() {
                                if tx
                                    .unbounded_send(Ok(querymt::chat::StreamChunk::Text(
                                        content_delta.to_string(),
                                    )))
                                    .is_err()
                                {
                                    return Ok((
                                        Usage {
                                            input_tokens: tokens.len() as u32,
                                            output_tokens,
                                            cache_read: 0,
                                            cache_write: 0,
                                            reasoning_tokens: 0,
                                        },
                                        !tool_calls_in_progress.is_empty(),
                                    ));
                                }
                            }
                        }

                        // Handle reasoning_content delta (thinking)
                        if let Some(reasoning_delta) =
                            delta.get("reasoning_content").and_then(|v| v.as_str())
                        {
                            if !reasoning_delta.is_empty() {
                                if tx
                                    .unbounded_send(Ok(querymt::chat::StreamChunk::Thinking(
                                        reasoning_delta.to_string(),
                                    )))
                                    .is_err()
                                {
                                    return Ok((
                                        Usage {
                                            input_tokens: tokens.len() as u32,
                                            output_tokens,
                                            cache_read: 0,
                                            cache_write: 0,
                                            reasoning_tokens: 0,
                                        },
                                        !tool_calls_in_progress.is_empty(),
                                    ));
                                }
                            }
                        }

                        // Handle tool call deltas - parse tool_calls array
                        if let Some(tool_calls_arr) =
                            delta.get("tool_calls").and_then(|v| v.as_array())
                        {
                            for tc in tool_calls_arr {
                                let index =
                                    tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

                                // Check if this is a new tool call (has id and name)
                                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                    let name = tc
                                        .get("function")
                                        .and_then(|f| f.get("name"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");

                                    tool_calls_in_progress.insert(
                                        index,
                                        (id.to_string(), name.to_string(), String::new()),
                                    );

                                    if tx
                                        .unbounded_send(Ok(
                                            querymt::chat::StreamChunk::ToolUseStart {
                                                index,
                                                id: id.to_string(),
                                                name: name.to_string(),
                                            },
                                        ))
                                        .is_err()
                                    {
                                        return Ok((
                                            Usage {
                                                input_tokens: tokens.len() as u32,
                                                output_tokens,
                                                cache_read: 0,
                                                cache_write: 0,
                                                reasoning_tokens: 0,
                                            },
                                            !tool_calls_in_progress.is_empty(),
                                        ));
                                    }
                                }

                                // Always check for arguments delta
                                if let Some(args) = tc
                                    .get("function")
                                    .and_then(|f| f.get("arguments"))
                                    .and_then(|v| v.as_str())
                                {
                                    if !args.is_empty() {
                                        if let Some(entry) = tool_calls_in_progress.get_mut(&index)
                                        {
                                            entry.2.push_str(args);
                                        }

                                        if tx
                                            .unbounded_send(Ok(
                                                querymt::chat::StreamChunk::ToolUseInputDelta {
                                                    index,
                                                    partial_json: args.to_string(),
                                                },
                                            ))
                                            .is_err()
                                        {
                                            return Ok((
                                                Usage {
                                                    input_tokens: tokens.len() as u32,
                                                    output_tokens,
                                                    cache_read: 0,
                                                    cache_write: 0,
                                                    reasoning_tokens: 0,
                                                },
                                                !tool_calls_in_progress.is_empty(),
                                            ));
                                        }
                                    }
                                }
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

        if stop_now {
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
        if !stop.is_empty() && generated_text.ends_with(stop) {
            let new_len = generated_text.len().saturating_sub(stop.len());
            generated_text.truncate(new_len);
            break;
        }
    }

    // Parse final response to get complete tool calls
    let (_, _, tool_calls, _) = parse_tool_response(result, &generated_text)?;

    // Emit ToolUseComplete for each tool call
    let has_tool_calls = if let Some(calls) = tool_calls {
        for (index, call) in calls.into_iter().enumerate() {
            if tx
                .unbounded_send(Ok(querymt::chat::StreamChunk::ToolUseComplete {
                    index,
                    tool_call: call,
                }))
                .is_err()
            {
                break;
            }
        }
        true
    } else {
        false
    };

    Ok((
        Usage {
            input_tokens: tokens.len() as u32,
            output_tokens,
            cache_read: 0,
            cache_write: 0,
            reasoning_tokens: 0,
        },
        has_tool_calls,
    ))
}
