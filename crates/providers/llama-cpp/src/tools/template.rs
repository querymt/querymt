use crate::config::LlamaCppConfig;
use llama_cpp_2::model::{ChatTemplateResult, LlamaChatTemplate, LlamaModel};
use llama_cpp_2::openai::OpenAIChatTemplateParams;
use querymt::chat::{ChatMessage, ChatRole, MessageType, Tool};
use querymt::error::LLMError;
use serde_json::Value;
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
pub(crate) fn build_messages_json_for_tools(
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
) -> Result<String, LLMError> {
    let mut json_messages = Vec::new();

    // Add system message if configured
    if !cfg.system.is_empty() {
        let system = cfg.system.join("\n\n");
        json_messages.push(serde_json::json!({
            "role": "system",
            "content": system
        }));
    }

    for msg in messages {
        match &msg.message_type {
            MessageType::Text => {
                let role = match msg.role {
                    ChatRole::User => "user",
                    ChatRole::Assistant => "assistant",
                };

                // For assistant messages, separate <think> blocks from content
                // into reasoning_content for the template engine.
                // If thinking was already extracted (msg.thinking is Some), use it.
                // Otherwise, extract from content as a fallback for messages
                // stored before thinking extraction was available.
                let (thinking, content) = if msg.thinking.is_some() {
                    (msg.thinking.clone(), msg.content.clone())
                } else if matches!(msg.role, ChatRole::Assistant) {
                    let (t, c) = querymt::chat::extract_thinking(&msg.content);
                    (t, c)
                } else {
                    (None, msg.content.clone())
                };

                let mut json_msg = serde_json::json!({
                    "role": role,
                    "content": content
                });
                if let Some(ref t) = thinking {
                    if !t.is_empty() {
                        json_msg["reasoning_content"] = serde_json::json!(t);
                    }
                }
                json_messages.push(json_msg);
            }
            MessageType::ToolUse(tool_calls) => {
                // Assistant message with tool calls in OpenAI format
                let tool_calls_array: Vec<Value> = tool_calls
                    .iter()
                    .map(|tc| {
                        serde_json::json!({
                            "id": tc.id,
                            "type": tc.call_type,
                            "function": {
                                "name": tc.function.name,
                                "arguments": tc.function.arguments
                            }
                        })
                    })
                    .collect();

                // Separate <think> blocks from content (fallback extraction)
                let (thinking, clean_content) = if msg.thinking.is_some() {
                    (msg.thinking.clone(), msg.content.clone())
                } else {
                    let (t, c) = querymt::chat::extract_thinking(&msg.content);
                    (t, c)
                };

                let content = if clean_content.is_empty() {
                    Value::Null
                } else {
                    Value::String(clean_content)
                };

                let mut json_msg = serde_json::json!({
                    "role": "assistant",
                    "content": content,
                    "tool_calls": tool_calls_array
                });
                if let Some(ref t) = thinking {
                    if !t.is_empty() {
                        json_msg["reasoning_content"] = serde_json::json!(t);
                    }
                }
                json_messages.push(json_msg);
            }
            MessageType::ToolResult(results) => {
                // Tool results - each result is a separate message with tool role
                // Note: function.arguments contains the result content,
                // function.name contains the tool name, and id is the tool_call_id
                for result in results {
                    json_messages.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": result.id,
                        "name": result.function.name,
                        "content": result.function.arguments
                    }));
                }
            }
            _ => {
                return Err(LLMError::InvalidRequest(
                    "Only text and tool-related messages are supported by llama.cpp provider"
                        .into(),
                ));
            }
        }
    }

    serde_json::to_string(&json_messages)
        .map_err(|e| LLMError::ProviderError(format!("Failed to serialize messages JSON: {}", e)))
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
