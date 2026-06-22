use crate::common_chat::ChatTemplateResult;
use crate::config::LlamaCppConfig;
use crate::messages;
use llama_cpp_2::common_chat::{CommonChatParams, CommonReasoningFormat};
use llama_cpp_2::model::{LlamaChatTemplate, LlamaModel};
use querymt::chat::{ChatMessage, Tool};
use querymt::error::LLMError;
use std::sync::Arc;

/// Utility function to escape regex special characters.
pub(crate) fn regex_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '.' | '^' | '$' | '|' | '(' | ')' | '*' | '+' | '?' | '[' | ']' | '{' | '}' | '\\' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// Utility function to anchor a regex pattern.
pub(crate) fn anchor_pattern(pattern: &str) -> String {
    if pattern.is_empty() {
        return "^$".to_string();
    }
    let mut anchored = String::new();
    if !pattern.starts_with('^') {
        anchored.push('^');
    }
    anchored.push_str(pattern);
    if !pattern.ends_with('$') {
        anchored.push('$');
    }
    anchored
}

/// Convert querymt Tool objects to OpenAI-compatible JSON string.
pub(crate) fn convert_tools_to_json(tools: &[Tool]) -> Result<String, LLMError> {
    serde_json::to_string(tools).map_err(|e| LLMError::ProviderError(e.to_string()))
}

/// Build OpenAI-compatible JSON messages from ChatMessage array for tool-aware conversations.
/// This now delegates to the unified messages module.
///
/// `media_marker` should be `Some(marker)` when the caller has a multimodal context
/// and needs image placeholder tokens injected into the prompt text.
pub(crate) fn build_messages_json_for_tools(
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
    media_marker: Option<&str>,
) -> Result<String, LLMError> {
    // Use the unified message conversion function
    let (json, _media_count) = messages::messages_to_json(cfg, messages, media_marker)?;
    Ok(json)
}

/// Apply chat template without tools, but with thinking support enabled.
///
/// Uses `apply_chat_template_oaicompat` (the same OAI-compat path as the tool-aware
/// template) so that the model's Jinja template can emit `<think>` blocks and the
/// returned `ChatTemplateResult` carries a properly initialised streaming parser.
/// The caller can then use `result.streaming_state_oaicompat()` to get a state machine
/// that routes `reasoning_content` deltas to `StreamChunk::Thinking` and `content`
/// deltas to `StreamChunk::Text`, without any manual tag parsing.
///
/// `media_marker` must be `Some(marker)` when the conversation contains image messages
/// so that placeholder tokens (e.g. `<__media__>`) are injected at the correct positions
/// in the prompt text before MTMD tokenization.
pub(crate) fn apply_template_for_thinking(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
    media_marker: Option<&str>,
) -> Result<ChatTemplateResult, LLMError> {
    let messages_json = build_messages_json_for_tools(cfg, messages, media_marker)?;

    log::debug!(
        "Applying chat template for thinking with {} messages",
        messages.len()
    );

    // Serialize the structured output schema to a JSON string for the FFI.
    // When a grammar is active it constrains all output, so thinking blocks
    // would violate the JSON grammar — disable thinking automatically.
    let json_schema_str = cfg
        .json_schema
        .as_ref()
        .and_then(|s| s.schema.as_ref())
        .and_then(|v| serde_json::to_string(v).ok());
    let has_schema = json_schema_str.is_some();

    let template = model
        .chat_template(cfg.chat_template.as_deref())
        .or_else(|_| LlamaChatTemplate::new("chatml"))
        .map_err(|e| LLMError::ProviderError(format!("Failed to get chat template: {}", e)))?;

    let mut params = CommonChatParams::new(&messages_json);
    params.json_schema = json_schema_str.as_deref();
    params.reasoning_format = CommonReasoningFormat::Auto;
    params.enable_thinking = if has_schema {
        false
    } else {
        cfg.enable_thinking.unwrap_or(true)
    };

    let result: ChatTemplateResult = model
        .apply_common_chat_template(Some(&template), &params)
        .map_err(|e| LLMError::ProviderError(format!("Failed to apply chat template: {}", e)))?
        .into();

    log::debug!(
        "Template applied (thinking): prompt_len={}",
        result.prompt.len(),
    );

    Ok(result)
}

/// Apply chat template with tools to generate prompt and grammar.
pub(crate) fn apply_template_with_tools(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
    tools: &[Tool],
    media_marker: Option<&str>,
) -> Result<ChatTemplateResult, LLMError> {
    let tools_json = convert_tools_to_json(tools)?;
    let messages_json = build_messages_json_for_tools(cfg, messages, media_marker)?;

    log::debug!(
        "Applying chat template with {} messages and {} tools",
        messages.len(),
        tools.len()
    );
    log::debug!("Messages JSON: {}", messages_json);
    log::debug!("Tools JSON: {}", tools_json);

    let json_schema_str = cfg
        .json_schema
        .as_ref()
        .and_then(|s| s.schema.as_ref())
        .and_then(|v| serde_json::to_string(v).ok());
    let has_schema = json_schema_str.is_some();

    let template = model
        .chat_template(cfg.chat_template.as_deref())
        .or_else(|_| LlamaChatTemplate::new("chatml"))
        .map_err(|e| LLMError::ProviderError(format!("Failed to get chat template: {}", e)))?;

    let mut params = CommonChatParams::new(&messages_json);
    params.tools_json = Some(&tools_json);
    params.json_schema = json_schema_str.as_deref();
    params.reasoning_format = CommonReasoningFormat::Auto;
    params.enable_thinking = if has_schema {
        false
    } else {
        cfg.enable_thinking.unwrap_or(true)
    };

    let result: ChatTemplateResult = model
        .apply_common_chat_template(Some(&template), &params)
        .map_err(|e| LLMError::ProviderError(format!("Failed to apply chat template: {}", e)))?
        .into();

    log::debug!(
        "Template applied: prompt_len={}, has_grammar={}, triggers={}, stops={}, parse_tool_calls={}",
        result.prompt.len(),
        result.grammar.is_some(),
        result.grammar_triggers.len(),
        result.additional_stops.len(),
        !tools.is_empty()
    );

    Ok(result)
}
