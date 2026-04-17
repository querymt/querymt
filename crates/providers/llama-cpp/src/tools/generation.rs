use crate::config::LlamaCppConfig;
use crate::multimodal::MultimodalContext;
use crate::response::GeneratedText;
use crate::tools::prefill::prefill_for_tool_generation;
use crate::tools::sampler::{SamplingParams, build_tool_sampler};
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

    let params = SamplingParams::from_config(cfg, temperature);
    let mut sampler = build_tool_sampler(model, result, &params);
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

    extract_parsed_response(&parsed)
}

/// Extract content, thinking, tool calls and finish reason from a parsed
/// OAI-compat JSON value.
///
/// Separated from [`parse_tool_response`] so the JSON-processing logic can be
/// unit-tested without requiring a live `ChatTemplateResult` / FFI context.
fn extract_parsed_response(
    parsed: &Value,
) -> Result<
    (
        String,
        Option<String>,
        Option<Vec<querymt::ToolCall>>,
        querymt::chat::FinishReason,
    ),
    LLMError,
> {
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

    // The llama.cpp C++ parser (chat.cpp) returns `arguments` as a parsed JSON
    // object (via `json::parse()`), but `querymt::FunctionCall::arguments` is a
    // `String`.  A plain `serde_json::from_value` would silently fail on the
    // type mismatch.  Instead we manually extract each field and stringify
    // `arguments` when it is not already a string — matching what every other
    // provider (anthropic, openai, google, ollama, …) does.
    let tool_calls: Option<Vec<querymt::ToolCall>> = parsed
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            if arr.is_empty() {
                return None;
            }
            let calls: Vec<querymt::ToolCall> = arr
                .iter()
                .filter_map(|tc| {
                    let id = tc.get("id")?.as_str()?.to_string();
                    let call_type = tc
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("function")
                        .to_string();
                    let func = tc.get("function")?;
                    let name = func.get("name")?.as_str()?.to_string();
                    let arguments = match func.get("arguments") {
                        Some(Value::String(s)) => s.clone(),
                        Some(v) => serde_json::to_string(v).unwrap_or_default(),
                        None => String::new(),
                    };
                    Some(querymt::ToolCall {
                        id,
                        call_type,
                        function: querymt::FunctionCall { name, arguments },
                    })
                })
                .collect();
            if calls.is_empty() { None } else { Some(calls) }
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

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::FinishReason;
    use serde_json::json;

    /// Regression test: llama.cpp's C++ parser returns `arguments` as a JSON
    /// object (via `json::parse()`).  Before the fix, `serde_json::from_value`
    /// silently failed because `FunctionCall::arguments` is a `String`, and
    /// `.ok()` swallowed the error — dropping tool calls entirely.
    #[test]
    fn tool_calls_with_object_arguments() {
        let parsed = json!({
            "role": "assistant",
            "content": "Let me search for files.",
            "tool_calls": [{
                "type": "function",
                "function": {
                    "name": "glob",
                    "arguments": {
                        "pattern": "**/*delegat*",
                        "path": "/some/path"
                    }
                },
                "id": "call_abc123"
            }]
        });

        let (content, thinking, tool_calls, finish_reason) =
            extract_parsed_response(&parsed).unwrap();

        assert_eq!(content, "Let me search for files.");
        assert!(thinking.is_none());
        assert_eq!(finish_reason, FinishReason::ToolCalls);

        let calls = tool_calls.expect("tool_calls should be Some");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_abc123");
        assert_eq!(calls[0].call_type, "function");
        assert_eq!(calls[0].function.name, "glob");

        // Arguments should be a JSON string representation of the object
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["pattern"], "**/*delegat*");
        assert_eq!(args["path"], "/some/path");
    }

    /// When `arguments` is already a JSON string (e.g. from providers that
    /// conform to the OpenAI convention), it must be preserved as-is.
    #[test]
    fn tool_calls_with_string_arguments() {
        let parsed = json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "type": "function",
                "function": {
                    "name": "read_file",
                    "arguments": "{\"path\":\"/tmp/test.txt\"}"
                },
                "id": "call_def456"
            }]
        });

        let (_, _, tool_calls, finish_reason) = extract_parsed_response(&parsed).unwrap();

        assert_eq!(finish_reason, FinishReason::ToolCalls);
        let calls = tool_calls.expect("tool_calls should be Some");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, "{\"path\":\"/tmp/test.txt\"}");
    }

    /// Tool call with empty/missing arguments (e.g. a no-argument tool).
    #[test]
    fn tool_calls_with_empty_object_arguments() {
        let parsed = json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "type": "function",
                "function": {
                    "name": "get_time",
                    "arguments": {}
                },
                "id": "call_empty"
            }]
        });

        let (_, _, tool_calls, finish_reason) = extract_parsed_response(&parsed).unwrap();

        assert_eq!(finish_reason, FinishReason::ToolCalls);
        let calls = tool_calls.expect("tool_calls should be Some");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "get_time");
        assert_eq!(calls[0].function.arguments, "{}");
    }

    /// Tool call with no `arguments` key at all.
    #[test]
    fn tool_calls_with_missing_arguments() {
        let parsed = json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "type": "function",
                "function": {
                    "name": "get_time"
                },
                "id": "call_noargs"
            }]
        });

        let (_, _, tool_calls, finish_reason) = extract_parsed_response(&parsed).unwrap();

        assert_eq!(finish_reason, FinishReason::ToolCalls);
        let calls = tool_calls.expect("tool_calls should be Some");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.arguments, "");
    }

    /// Multiple tool calls in a single response.
    #[test]
    fn multiple_tool_calls() {
        let parsed = json!({
            "role": "assistant",
            "content": "I'll search in parallel.",
            "tool_calls": [
                {
                    "type": "function",
                    "function": {
                        "name": "glob",
                        "arguments": {"pattern": "**/*.rs"}
                    },
                    "id": "call_1"
                },
                {
                    "type": "function",
                    "function": {
                        "name": "search_text",
                        "arguments": {"pattern": "TODO", "path": "/src"}
                    },
                    "id": "call_2"
                }
            ]
        });

        let (content, _, tool_calls, finish_reason) = extract_parsed_response(&parsed).unwrap();

        assert_eq!(content, "I'll search in parallel.");
        assert_eq!(finish_reason, FinishReason::ToolCalls);
        let calls = tool_calls.expect("tool_calls should be Some");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "glob");
        assert_eq!(calls[1].function.name, "search_text");
    }

    /// No tool calls → finish_reason should be Stop.
    #[test]
    fn no_tool_calls() {
        let parsed = json!({
            "role": "assistant",
            "content": "Here is my answer."
        });

        let (content, _, tool_calls, finish_reason) = extract_parsed_response(&parsed).unwrap();

        assert_eq!(content, "Here is my answer.");
        assert!(tool_calls.is_none());
        assert_eq!(finish_reason, FinishReason::Stop);
    }

    /// Empty tool_calls array → treated as no tool calls.
    #[test]
    fn empty_tool_calls_array() {
        let parsed = json!({
            "role": "assistant",
            "content": "Done.",
            "tool_calls": []
        });

        let (_, _, tool_calls, finish_reason) = extract_parsed_response(&parsed).unwrap();

        assert!(tool_calls.is_none());
        assert_eq!(finish_reason, FinishReason::Stop);
    }

    /// reasoning_content is used when present.
    #[test]
    fn reasoning_content_extracted() {
        let parsed = json!({
            "role": "assistant",
            "content": "The answer is 42.",
            "reasoning_content": "Let me think about this step by step..."
        });

        let (content, thinking, tool_calls, finish_reason) =
            extract_parsed_response(&parsed).unwrap();

        assert_eq!(content, "The answer is 42.");
        assert_eq!(
            thinking.as_deref(),
            Some("Let me think about this step by step...")
        );
        assert!(tool_calls.is_none());
        assert_eq!(finish_reason, FinishReason::Stop);
    }
}
