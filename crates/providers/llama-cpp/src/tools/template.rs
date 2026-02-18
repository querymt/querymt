use crate::config::LlamaCppConfig;
use crate::messages;
use llama_cpp_2::model::{ChatTemplateResult, LlamaChatTemplate, LlamaModel};
use llama_cpp_2::openai::OpenAIChatTemplateParams;
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
pub(crate) fn build_messages_json_for_tools(
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
) -> Result<String, LLMError> {
    // Use the unified message conversion function
    messages::messages_to_json(cfg, messages)
}

/// Apply chat template without tools, but with thinking support enabled.
///
/// Uses `apply_chat_template_oaicompat` (the same OAI-compat path as the tool-aware
/// template) so that the model's Jinja template can emit `<think>` blocks and the
/// returned `ChatTemplateResult` carries a properly initialised streaming parser.
/// The caller can then use `result.streaming_state_oaicompat()` to get a state machine
/// that routes `reasoning_content` deltas to `StreamChunk::Thinking` and `content`
/// deltas to `StreamChunk::Text`, without any manual tag parsing.
pub(crate) fn apply_template_for_thinking(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
) -> Result<ChatTemplateResult, LLMError> {
    let messages_json = build_messages_json_for_tools(cfg, messages)?;

    log::debug!(
        "Applying chat template for thinking with {} messages",
        messages.len()
    );

    let template = model
        .chat_template(cfg.chat_template.as_deref())
        .or_else(|_| LlamaChatTemplate::new("chatml"))
        .map_err(|e| LLMError::ProviderError(format!("Failed to get chat template: {}", e)))?;

    let params = OpenAIChatTemplateParams {
        messages_json: &messages_json,
        tools_json: None,
        tool_choice: None,
        json_schema: None,
        grammar: None,
        reasoning_format: None,
        chat_template_kwargs: None,
        add_generation_prompt: true,
        use_jinja: true,
        parallel_tool_calls: false,
        enable_thinking: cfg.enable_thinking.unwrap_or(true),
        add_bos: false,
        add_eos: false,
        parse_tool_calls: false,
    };

    let result = model
        .apply_chat_template_oaicompat(&template, &params)
        .map_err(|e| LLMError::ProviderError(format!("Failed to apply chat template: {}", e)))?;

    log::debug!(
        "Template applied (thinking): prompt_len={}, thinking_forced_open={}",
        result.prompt.len(),
        result.thinking_forced_open,
    );

    Ok(result)
}

/// Apply chat template with tools to generate prompt and grammar.
pub(crate) fn apply_template_with_tools(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
    tools: &[Tool],
) -> Result<ChatTemplateResult, LLMError> {
    let tools_json = convert_tools_to_json(tools)?;
    let messages_json = build_messages_json_for_tools(cfg, messages)?;

    log::debug!(
        "Applying chat template with {} messages and {} tools",
        messages.len(),
        tools.len()
    );
    log::debug!("Messages JSON: {}", messages_json);
    log::debug!("Tools JSON: {}", tools_json);

    let template = model
        .chat_template(cfg.chat_template.as_deref())
        .or_else(|_| LlamaChatTemplate::new("chatml"))
        .map_err(|e| LLMError::ProviderError(format!("Failed to get chat template: {}", e)))?;

    let params = OpenAIChatTemplateParams {
        messages_json: &messages_json,
        tools_json: Some(&tools_json),
        tool_choice: None,
        json_schema: None,
        grammar: None,
        reasoning_format: None,
        chat_template_kwargs: None,
        add_generation_prompt: true,
        use_jinja: true,
        parallel_tool_calls: false,
        enable_thinking: cfg.enable_thinking.unwrap_or(true),
        // BOS is handled by the tokenizer in generate_with_tools(),
        // not by the template engine, to avoid double-BOS.
        // See self.cfg.add_bos.
        add_bos: false,
        add_eos: false,
        parse_tool_calls: true,
    };

    let result = model
        .apply_chat_template_oaicompat(&template, &params)
        .map_err(|e| LLMError::ProviderError(format!("Failed to apply chat template: {}", e)))?;

    log::debug!(
        "Template applied: prompt_len={}, has_grammar={}, triggers={}, stops={}, parse_tool_calls={}",
        result.prompt.len(),
        result.grammar.is_some(),
        result.grammar_triggers.len(),
        result.additional_stops.len(),
        result.parse_tool_calls
    );

    Ok(result)
}
