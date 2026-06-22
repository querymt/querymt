use crate::chat_format::parse_assistant_format_with_state;
use crate::common_chat::ChatTemplateResult;
use crate::config::LlamaCppConfig;
use crate::multimodal::MultimodalContext;
use crate::response::GeneratedText;
use crate::tools::prefill::prefill_for_tool_generation;
use crate::tools::sampler::{SamplingParams, build_tool_sampler};
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::mtmd::MtmdBitmap;
use querymt::Usage;
use querymt::error::LLMError;
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

    let params = SamplingParams::from_config(cfg, temperature);
    let mut sampler = build_tool_sampler(model, result, &params)?;
    let mut output_tokens = 0u32;
    let mut output = String::new();
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut first_token_logged = false;
    let mut eog_hit = false;

    log::debug!(
        "generate_with_tools: sampler built, has_grammar={}, input_tokens={}, max_tokens={}",
        result.grammar.is_some(),
        state.input_tokens,
        max_tokens
    );

    while state.n_cur < state.n_len_total {
        let token = sampler.sample(&state.ctx, batch.n_tokens() - 1);
        if model.is_eog_token(token) {
            eog_hit = true;
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

        if !first_token_logged {
            first_token_logged = true;
            log::debug!(
                "generate_with_tools: first token id={}, piece=<<<{}>>>, special={}",
                token,
                chunk,
                special
            );
        }

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

    let head_len = 400.min(output.len());
    let tail_len = 400.min(output.len());
    let head = &output[..head_len];
    let tail = if output.len() > 400 {
        &output[output.len() - tail_len..]
    } else {
        ""
    };
    log::debug!(
        "generate_with_tools: done output_tokens={}, eog_hit={}, output_len={}, head=<<<{}>>>, tail=<<<{}>>>",
        output_tokens,
        eog_hit,
        output.len(),
        head,
        tail
    );

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

    extract_parsed_response(text, result.starts_in_thinking)
}

/// Extract content, thinking, tool calls and finish reason from a parsed
/// OAI-compat JSON value.
///
/// Separated from [`parse_tool_response`] so the JSON-processing logic can be
/// unit-tested without requiring a live `ChatTemplateResult` / FFI context.
fn extract_parsed_response(
    text: &str,
    starts_in_thinking: bool,
) -> Result<
    (
        String,
        Option<String>,
        Option<Vec<querymt::ToolCall>>,
        querymt::chat::FinishReason,
    ),
    LLMError,
> {
    let parsed = parse_assistant_format_with_state(text, starts_in_thinking);
    let finish_reason = if parsed.tool_calls.is_some() {
        querymt::chat::FinishReason::ToolCalls
    } else {
        querymt::chat::FinishReason::Stop
    };

    log::debug!(
        "Parsed response: content_len={}, thinking={}, tool_calls={}, finish_reason={:?}",
        parsed.content.len(),
        parsed.thinking.as_ref().map(|t| t.len()).unwrap_or(0),
        parsed
            .tool_calls
            .as_ref()
            .map(|tc: &Vec<querymt::ToolCall>| tc.len())
            .unwrap_or(0),
        finish_reason
    );

    Ok((
        parsed.content,
        parsed.thinking,
        parsed.tool_calls,
        finish_reason,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::FinishReason;

    #[test]
    fn parses_plain_text_response() {
        let (content, thinking, tool_calls, finish_reason) =
            extract_parsed_response("Here is my answer.", false).unwrap();

        assert_eq!(content, "Here is my answer.");
        assert!(thinking.is_none());
        assert!(tool_calls.is_none());
        assert_eq!(finish_reason, FinishReason::Stop);
    }

    #[test]
    fn parses_thinking_and_qwen_tool_call() {
        let input = r#"<think>Need a file search.</think>
<tool_call>{"name":"glob","arguments":{"pattern":"**/*.rs"}}</tool_call>"#;
        let (content, thinking, tool_calls, finish_reason) =
            extract_parsed_response(input, false).unwrap();

        assert!(content.is_empty());
        assert_eq!(thinking.as_deref(), Some("Need a file search."));
        assert_eq!(finish_reason, FinishReason::ToolCalls);
        let calls = tool_calls.expect("tool_calls should be Some");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "glob");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["pattern"], "**/*.rs");
    }

    #[test]
    fn parses_qwen_function_tool_call() {
        let input = "<tool_call>\n<function=get_weather>\n<parameter=city>\nCopenhagen\n</parameter>\n</function>\n</tool_call>";
        let (_, _, tool_calls, finish_reason) = extract_parsed_response(input, false).unwrap();

        assert_eq!(finish_reason, FinishReason::ToolCalls);
        let calls = tool_calls.expect("tool_calls should be Some");
        assert_eq!(calls[0].function.name, "get_weather");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["city"], "Copenhagen");
    }

    #[test]
    fn parses_open_prompt_thinking_then_tool_call() {
        let input = "Thinking Process:\n1. analyze\n</think><tool_call>{\"name\":\"glob\",\"arguments\":{\"pattern\":\"**/*.rs\"}}</tool_call>";
        let (content, thinking, tool_calls, finish_reason) =
            extract_parsed_response(input, true).unwrap();

        assert!(content.is_empty());
        assert_eq!(thinking.as_deref(), Some("Thinking Process:\n1. analyze"));
        assert_eq!(finish_reason, FinishReason::ToolCalls);
        assert_eq!(tool_calls.unwrap()[0].function.name, "glob");
    }
}
