use crate::chat_format::ParsedDelta;
use crate::common_chat::ChatTemplateResult;
use crate::config::LlamaCppConfig;
use crate::multimodal::MultimodalContext;
use crate::tools::generation::parse_tool_response;
use crate::tools::prefill::prefill_for_tool_generation;
use crate::tools::sampler::{SamplingParams, build_tool_sampler};
use futures::channel::mpsc;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::mtmd::MtmdBitmap;
use querymt::Usage;
use querymt::error::LLMError;
use std::collections::HashSet;
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
    mm_ctx: Option<&MultimodalContext>,
    bitmaps: &[MtmdBitmap],
) -> Result<(Usage, bool), LLMError> {
    let mut state =
        prefill_for_tool_generation(model, cfg, &result.prompt, max_tokens, mm_ctx, bitmaps)?;

    log::debug!(
        "Streaming generation with tools: input_tokens={}, max_tokens={}, has_multimodal={}",
        state.input_tokens,
        max_tokens,
        mm_ctx.is_some() && !bitmaps.is_empty()
    );

    if max_tokens == 0 {
        return Ok((
            Usage {
                input_tokens: state.input_tokens,
                output_tokens: 0,
                cache_read: 0,
                cache_write: 0,
                reasoning_tokens: 0,
            },
            false,
        ));
    }

    let mut batch = LlamaBatch::new(state.n_batch, 1);

    let mut preserved = HashSet::new();
    for token_str in &result.preserved_tokens {
        if let Ok(preserved_tokens) = model.str_to_token(token_str, AddBos::Never) {
            if preserved_tokens.len() == 1 {
                preserved.insert(preserved_tokens[0]);
            }
        }
    }

    let mut stream_state = result.streaming_state();
    let params = SamplingParams::from_config(cfg, temperature);
    let mut sampler = build_tool_sampler(model, result, &params)?;
    let mut output_tokens = 0u32;
    let mut generated_text = String::new();
    let mut decoder = encoding_rs::UTF_8.new_decoder();

    while state.n_cur < state.n_len_total {
        let token = sampler.sample(&state.ctx, batch.n_tokens() - 1);
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

        let stop_now = result
            .additional_stops
            .iter()
            .any(|stop| !stop.is_empty() && generated_text.ends_with(stop));

        for delta in stream_state.update(&chunk, !stop_now) {
            // In tool-capable streaming, buffer normal text until final parse so
            // partially generated tool syntax never leaks to the UI.
            if let ParsedDelta::Thinking(thinking) = delta {
                if tx
                    .unbounded_send(Ok(querymt::chat::StreamChunk::Thinking(thinking)))
                    .is_err()
                {
                    return Ok((
                        Usage {
                            input_tokens: state.input_tokens,
                            output_tokens,
                            cache_read: 0,
                            cache_write: 0,
                            reasoning_tokens: 0,
                        },
                        false,
                    ));
                }
            }
        }

        if stop_now {
            break;
        }

        batch.clear();
        batch
            .add(token, state.n_cur, &[0], true)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        state.n_cur += 1;
        output_tokens += 1;

        state
            .ctx
            .decode(&mut batch)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;
    }

    for stop in &result.additional_stops {
        if !stop.is_empty() && generated_text.ends_with(stop) {
            let new_len = generated_text.len().saturating_sub(stop.len());
            generated_text.truncate(new_len);
            break;
        }
    }

    for delta in stream_state.finish() {
        if let ParsedDelta::Thinking(thinking) = delta {
            if tx
                .unbounded_send(Ok(querymt::chat::StreamChunk::Thinking(thinking)))
                .is_err()
            {
                break;
            }
        }
    }

    let (content, _, tool_calls, _) = parse_tool_response(result, &generated_text)?;
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
        if !content.is_empty() {
            let _ = tx.unbounded_send(Ok(querymt::chat::StreamChunk::Text(content)));
        }
        false
    };

    Ok((
        Usage {
            input_tokens: state.input_tokens,
            output_tokens,
            cache_read: 0,
            cache_write: 0,
            reasoning_tokens: 0,
        },
        has_tool_calls,
    ))
}
