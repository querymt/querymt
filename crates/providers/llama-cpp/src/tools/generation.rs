use crate::config::LlamaCppConfig;
use crate::multimodal::MultimodalContext;
use crate::response::GeneratedText;
use crate::tools::prefill::prefill_for_tool_generation;
use crate::tools::sampler::build_tool_sampler;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::{AddBos, ChatTemplateResult, LlamaModel};
use llama_cpp_2::mtmd::MtmdBitmap;
use querymt::Usage;
use querymt::error::LLMError;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

/// Generate text with grammar-constrained sampling for tool calls.
pub(crate) fn generate_with_tools(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    result: &ChatTemplateResult,
    max_tokens: u32,
    temperature: Option<f32>,
    mm_ctx: Option<&MultimodalContext>,
    bitmaps: &[MtmdBitmap],
) -> Result<GeneratedText, LLMError> {
    let mut state =
        prefill_for_tool_generation(model, cfg, &result.prompt, max_tokens, mm_ctx, bitmaps)?;

    log::debug!(
        "Generating with tools: input_tokens={}, max_tokens={}, has_multimodal={}",
        state.input_tokens,
        max_tokens,
        mm_ctx.is_some() && !bitmaps.is_empty()
    );

    if max_tokens == 0 {
        return Ok(GeneratedText {
            text: String::new(),
            usage: Usage {
                input_tokens: state.input_tokens,
                output_tokens: 0,
                cache_read: 0,
                cache_write: 0,
                reasoning_tokens: 0,
            },
        });
    }

    let mut batch = LlamaBatch::new(state.n_batch, 1);

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
    let mut sampler = build_tool_sampler(
        model,
        result,
        temperature,
        seed,
        cfg.top_p,
        cfg.top_k,
        cfg.min_p,
    );
    let mut output_tokens = 0u32;
    let mut output = String::new();
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
            .add(token, state.n_cur, &[0], true)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        state.n_cur += 1;
        output_tokens += 1;

        state
            .ctx
            .decode(&mut batch)
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
            input_tokens: state.input_tokens,
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
