use crate::chat_format::ToolFormat;
use crate::common_chat::{ChatTemplateResult, ReasoningFormat, prompt_starts_in_thinking};
use crate::config::LlamaCppConfig;
use crate::messages;
use llama_cpp_2::model::LlamaModel;
use minijinja::Environment;
use querymt::chat::{ChatMessage, ReasoningEffort, Tool};
use querymt::error::LLMError;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::sync::{Arc, LazyLock};

static MINIJINJA_ENV: LazyLock<Environment<'static>> = LazyLock::new(|| {
    let mut env = Environment::new();
    env.add_function(
        "raise_exception",
        |msg: String| -> Result<(), minijinja::Error> {
            Err(minijinja::Error::new(
                minijinja::ErrorKind::InvalidOperation,
                msg,
            ))
        },
    );
    env.add_function("strftime_now", strftime_now);
    env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
    env
});

fn strftime_now(format: String) -> String {
    chrono::Local::now().format(&format).to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TemplateReasoningScale {
    Unsupported,
    LowMediumXHigh,
    LowMediumHighXHigh,
}

#[derive(Serialize)]
struct ChatTemplateContext {
    messages: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Value>,
    add_generation_prompt: bool,
    bos_token: String,
    eos_token: String,
    enable_thinking: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
}

fn template_mentions_quoted_value(template: &str, value: &str) -> bool {
    template.contains(&format!("'{value}'")) || template.contains(&format!("\"{value}\""))
}

fn detect_reasoning_scale(template: &str) -> TemplateReasoningScale {
    if !template.contains("reasoning_effort") {
        return TemplateReasoningScale::Unsupported;
    }

    let has_low = template_mentions_quoted_value(template, "low");
    let has_medium = template_mentions_quoted_value(template, "medium");
    let has_high = template_mentions_quoted_value(template, "high");
    let has_xhigh = template_mentions_quoted_value(template, "xhigh");

    match (has_low, has_medium, has_high, has_xhigh) {
        (true, true, false, true) => TemplateReasoningScale::LowMediumXHigh,
        (true, true, true, true) => TemplateReasoningScale::LowMediumHighXHigh,
        _ => TemplateReasoningScale::Unsupported,
    }
}

fn map_reasoning_effort(
    effort: ReasoningEffort,
    scale: TemplateReasoningScale,
) -> Option<&'static str> {
    match scale {
        TemplateReasoningScale::Unsupported => None,
        TemplateReasoningScale::LowMediumXHigh => Some(match effort {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High | ReasoningEffort::Max => "xhigh",
        }),
        TemplateReasoningScale::LowMediumHighXHigh => Some(match effort {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Max => "xhigh",
        }),
    }
}

fn resolve_reasoning_effort(
    configured: Option<ReasoningEffort>,
    enable_thinking: bool,
    scale: TemplateReasoningScale,
) -> Option<&'static str> {
    configured
        .filter(|_| enable_thinking)
        .and_then(|effort| map_reasoning_effort(effort, scale))
}

fn render_template_source(
    template: &str,
    context: &ChatTemplateContext,
) -> Result<String, LLMError> {
    MINIJINJA_ENV
        .template_from_str(template)
        .map_err(|e| LLMError::ProviderError(format!("Failed to compile chat template: {e}")))?
        .render(context)
        .map_err(|e| LLMError::ProviderError(format!("Failed to render chat template: {e}")))
}

fn message_shape_summary(messages: &Value) -> String {
    let Some(messages) = messages.as_array() else {
        return "invalid".to_string();
    };

    messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let content_type = match message.get("content") {
                Some(Value::String(_)) => "string",
                Some(Value::Null) => "null",
                Some(Value::Array(_)) => "array",
                Some(Value::Object(_)) => "object",
                Some(Value::Bool(_)) => "bool",
                Some(Value::Number(_)) => "number",
                None => "missing",
            };
            let tool_calls = message
                .get("tool_calls")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            format!("{index}:{role}(content={content_type},tool_calls={tool_calls})")
        })
        .collect::<Vec<_>>()
        .join(",")
}

pub(crate) fn apply_template_for_thinking(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
    media_marker: Option<&str>,
) -> Result<ChatTemplateResult, LLMError> {
    render_template(model, cfg, messages, None, media_marker)
}

pub(crate) fn apply_template_with_tools(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
    tools: &[Tool],
    media_marker: Option<&str>,
) -> Result<ChatTemplateResult, LLMError> {
    render_template(model, cfg, messages, Some(tools), media_marker)
}

fn render_template(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
    tools: Option<&[Tool]>,
    media_marker: Option<&str>,
) -> Result<ChatTemplateResult, LLMError> {
    let (messages_json, _) = messages::messages_to_json(cfg, messages, media_marker)?;
    let messages_value = serde_json::from_str(&messages_json)
        .map_err(|e| LLMError::ProviderError(format!("Failed to parse messages JSON: {e}")))?;
    let tools_value = tools
        .map(serde_json::to_value)
        .transpose()
        .map_err(|e| LLMError::ProviderError(format!("Failed to serialize tools: {e}")))?;

    let template = select_template(model, cfg, tools.is_some())?;
    let architecture = model.meta_val_str("general.architecture").ok();
    let model_name = model.meta_val_str("general.name").ok();
    let tool_format = tools.and_then(|_| {
        ToolFormat::detect(&template, architecture.as_deref(), model_name.as_deref())
    });
    let grammar = tool_format.and_then(|format| tools.and_then(|tools| format.grammar(tools)));
    log::debug!(
        "render_template: tools_count={}, tool_format={:?}, has_grammar={}, architecture={:?}, model_name={:?}",
        tools.map_or(0, |t| t.len()),
        tool_format,
        grammar.is_some(),
        architecture,
        model_name
    );

    let has_schema = cfg
        .json_schema
        .as_ref()
        .and_then(|s| s.schema.as_ref())
        .is_some();
    let enable_thinking = !has_schema && cfg.enable_thinking.unwrap_or(true);
    let reasoning_scale = detect_reasoning_scale(&template);
    let reasoning_effort =
        resolve_reasoning_effort(cfg.reasoning_effort, enable_thinking, reasoning_scale);
    if cfg.reasoning_effort.is_some()
        && enable_thinking
        && reasoning_scale == TemplateReasoningScale::Unsupported
    {
        log::warn!(
            "The selected chat template does not expose a supported reasoning_effort scale; ignoring configured effort"
        );
    }
    log::debug!(
        "render_template: has_schema={}, enable_thinking={} (cfg explicit: {:?}), reasoning_scale={:?}, reasoning_effort={:?}",
        has_schema,
        enable_thinking,
        cfg.enable_thinking,
        reasoning_scale,
        reasoning_effort
    );
    let add_generation_prompt = messages.last().map_or(true, |msg| {
        msg.role == querymt::chat::ChatRole::User
            || msg
                .content
                .iter()
                .any(|block| matches!(block, querymt::chat::Content::ToolResult { .. }))
    });
    let context = ChatTemplateContext {
        messages: messages_value,
        tools: tools_value,
        add_generation_prompt,
        bos_token: token_piece(model, model.token_bos()),
        eos_token: token_piece(model, model.token_eos()),
        enable_thinking,
        reasoning_effort,
    };
    let template = rewrite_generation_tags(&template);
    let prompt = render_template_source(&template, &context).map_err(|error| {
        LLMError::ProviderError(format!(
            "Chat template rendering failed (messages=[{}], tools={}): {error}",
            message_shape_summary(&context.messages),
            tools.map_or(0, <[Tool]>::len),
        ))
    })?;

    let reasoning_format = ReasoningFormat::detect(&prompt);
    let starts_in_thinking = prompt_starts_in_thinking(&prompt, reasoning_format);

    let prompt_tail_len = 1200.min(prompt.len());
    let prompt_tail = if prompt_tail_len > 0 {
        &prompt[prompt.len() - prompt_tail_len..]
    } else {
        ""
    };
    let tools_in_prompt = tools.is_some() && prompt.contains("tools");
    log::debug!(
        "render_template: prompt_len={}, starts_in_thinking={}, reasoning_format={:?}, tools_section_in_prompt={}, prompt_tail=<<<{}>>>",
        prompt.len(),
        starts_in_thinking,
        reasoning_format,
        tools_in_prompt,
        prompt_tail
    );

    Ok(ChatTemplateResult {
        prompt,
        grammar,
        preserved_tokens: known_preserved_tokens(),
        additional_stops: known_stop_sequences(),
        starts_in_thinking,
        reasoning_format,
    })
}

fn token_piece(model: &LlamaModel, token: llama_cpp_2::token::LlamaToken) -> String {
    model
        .token_to_piece(token, &mut encoding_rs::UTF_8.new_decoder(), true, None)
        .unwrap_or_default()
}

fn select_template(
    model: &Arc<LlamaModel>,
    cfg: &LlamaCppConfig,
    with_tools: bool,
) -> Result<String, LLMError> {
    if let Some(configured) = cfg.chat_template.as_deref() {
        log::debug!(
            "select_template: using configured chat_template (with_tools={}, len={})",
            with_tools,
            configured.len()
        );
        return Ok(configured.to_string());
    }

    if with_tools {
        match model.chat_template(Some("tool_use")) {
            Ok(template) => {
                let s = template.to_string().map_err(|e| {
                    LLMError::ProviderError(format!("Invalid tool_use template: {e}"))
                })?;
                let has_tools = s.contains("tools");
                log::debug!(
                    "select_template: using model 'tool_use' template (len={}, has_tools_keyword={})",
                    s.len(),
                    has_tools
                );
                return Ok(s);
            }
            Err(e) => {
                log::warn!(
                    "select_template: model has no 'tool_use' chat template ({}); falling back to default template",
                    e
                );
            }
        }
    }

    let default = model
        .chat_template(None)
        .and_then(|t| t.to_string().map_err(Into::into))
        .map_err(|e| LLMError::ProviderError(format!("Failed to get chat template: {e}")))?;
    let has_tools = default.contains("tools");
    log::debug!(
        "select_template: using model default template (len={}, with_tools={}, has_tools_keyword={})",
        default.len(),
        with_tools,
        has_tools
    );
    if with_tools && !has_tools {
        log::warn!(
            "select_template: tools requested but the default chat template does not mention 'tools' — the model will not see tool definitions"
        );
    }
    Ok(default)
}

fn rewrite_generation_tags(template: &str) -> String {
    static GENERATION_TAG: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\{%(-?)\s*(end)?generation\s*(-?)%\}")
            .expect("generation-tag regex should compile")
    });
    GENERATION_TAG
        .replace_all(template, |captures: &regex::Captures<'_>| {
            let keyword = if captures.get(2).is_some() {
                "endif"
            } else {
                "if true"
            };
            format!("{{%{} {keyword} {}%}}", &captures[1], &captures[3])
        })
        .into_owned()
}

fn known_preserved_tokens() -> Vec<String> {
    [
        "<tool_call>",
        "</tool_call>",
        "<|tool_call>",
        "<tool_call|>",
        "<|tool_call_start|>",
        "<|tool_call_end|>",
        "<start_function_call>",
        "<end_function_call>",
        "<think>",
        "</think>",
        "<|channel>",
        "<channel|>",
        "<|think|>",
        "<|\"|>",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn known_stop_sequences() -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const QWEN_TEMPLATE: &str = r#"
{%- if enable_thinking %}
    {%- set effort = reasoning_effort|default('xhigh') %}
    {%- if effort not in ('low', 'medium', 'xhigh') %}
        {{- raise_exception('unsupported effort') }}
    {%- endif %}
    {{- 'effort=' + effort }}
{%- else %}
    {{- 'thinking=disabled' }}
{%- endif %}
{%- if tools is defined %}{{- ';tools=' + (tools|length|string) }}{% endif -%}
"#;

    const FOUR_LEVEL_TEMPLATE: &str = r#"
{%- if reasoning_effort in ('low', 'medium', 'high', 'xhigh') -%}
{{- reasoning_effort -}}
{%- endif -%}
"#;

    fn context(
        enable_thinking: bool,
        reasoning_effort: Option<&'static str>,
        tools: Option<Value>,
    ) -> ChatTemplateContext {
        ChatTemplateContext {
            messages: serde_json::json!([]),
            tools,
            add_generation_prompt: true,
            bos_token: String::new(),
            eos_token: String::new(),
            enable_thinking,
            reasoning_effort,
        }
    }

    #[test]
    fn message_shape_summary_does_not_include_message_content() {
        let messages = serde_json::json!([
            {"role": "user", "content": "secret prompt"},
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [{"id": "call_1"}]
            },
            {"role": "tool", "content": "secret result"}
        ]);

        let summary = message_shape_summary(&messages);

        assert_eq!(
            summary,
            "0:user(content=string,tool_calls=0),1:assistant(content=string,tool_calls=1),2:tool(content=string,tool_calls=0)"
        );
        assert!(!summary.contains("secret"));
    }

    #[test]
    fn detects_supported_reasoning_scales() {
        assert_eq!(
            detect_reasoning_scale(QWEN_TEMPLATE),
            TemplateReasoningScale::LowMediumXHigh
        );
        assert_eq!(
            detect_reasoning_scale(FOUR_LEVEL_TEMPLATE),
            TemplateReasoningScale::LowMediumHighXHigh
        );
        assert_eq!(
            detect_reasoning_scale("{{ messages }}"),
            TemplateReasoningScale::Unsupported
        );
    }

    #[test]
    fn maps_portable_effort_to_template_scale() {
        let qwen = TemplateReasoningScale::LowMediumXHigh;
        assert_eq!(
            map_reasoning_effort(ReasoningEffort::Low, qwen),
            Some("low")
        );
        assert_eq!(
            map_reasoning_effort(ReasoningEffort::Medium, qwen),
            Some("medium")
        );
        assert_eq!(
            map_reasoning_effort(ReasoningEffort::High, qwen),
            Some("xhigh")
        );
        assert_eq!(
            map_reasoning_effort(ReasoningEffort::Max, qwen),
            Some("xhigh")
        );

        let four_level = TemplateReasoningScale::LowMediumHighXHigh;
        assert_eq!(
            map_reasoning_effort(ReasoningEffort::High, four_level),
            Some("high")
        );
        assert_eq!(
            map_reasoning_effort(ReasoningEffort::Max, four_level),
            Some("xhigh")
        );
    }

    #[test]
    fn omitted_effort_remains_undefined_and_uses_template_default() {
        let rendered = render_template_source(QWEN_TEMPLATE, &context(true, None, None)).unwrap();
        assert_eq!(rendered, "effort=xhigh");
    }

    #[test]
    fn explicit_effort_and_tools_share_the_same_context() {
        let rendered = render_template_source(
            QWEN_TEMPLATE,
            &context(
                true,
                Some("low"),
                Some(serde_json::json!([{"name": "tool"}])),
            ),
        )
        .unwrap();
        assert_eq!(rendered, "effort=low;tools=1");
    }

    #[test]
    fn disabled_thinking_suppresses_reasoning_effort() {
        let effort = resolve_reasoning_effort(
            Some(ReasoningEffort::Max),
            false,
            TemplateReasoningScale::LowMediumXHigh,
        );
        let rendered =
            render_template_source(QWEN_TEMPLATE, &context(false, effort, None)).unwrap();
        assert_eq!(rendered, "thinking=disabled");
    }
}
