use crate::chat_format::ToolFormat;
use crate::common_chat::{ChatTemplateResult, prompt_starts_in_thinking};
use crate::config::LlamaCppConfig;
use crate::messages;
use llama_cpp_2::model::LlamaModel;
use minijinja::{Environment, Value};
use querymt::chat::{ChatMessage, Tool};
use querymt::error::LLMError;
use regex::Regex;
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
    let messages_value: Value = serde_json::from_str::<serde_json::Value>(&messages_json)
        .map(Value::from_serialize)
        .map_err(|e| LLMError::ProviderError(format!("Failed to parse messages JSON: {e}")))?;

    let tools_value = tools
        .map(|tools| serde_json::to_value(tools))
        .transpose()
        .map_err(|e| LLMError::ProviderError(format!("Failed to serialize tools: {e}")))?
        .map(Value::from_serialize)
        .unwrap_or(Value::UNDEFINED);

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
    let template = rewrite_generation_tags(&template);
    let tmpl = MINIJINJA_ENV
        .template_from_str(&template)
        .map_err(|e| LLMError::ProviderError(format!("Failed to compile chat template: {e}")))?;

    let has_schema = cfg
        .json_schema
        .as_ref()
        .and_then(|s| s.schema.as_ref())
        .is_some();
    let enable_thinking = if has_schema {
        false
    } else {
        cfg.enable_thinking.unwrap_or(true)
    };
    log::debug!(
        "render_template: has_schema={}, enable_thinking={} (cfg explicit: {:?})",
        has_schema,
        enable_thinking,
        cfg.enable_thinking
    );
    let add_generation_prompt = messages.last().map_or(true, |msg| {
        msg.role == querymt::chat::ChatRole::User
            || msg
                .content
                .iter()
                .any(|block| matches!(block, querymt::chat::Content::ToolResult { .. }))
    });

    let prompt = tmpl
        .render(minijinja::context! {
            messages => messages_value,
            tools => tools_value,
            add_generation_prompt => add_generation_prompt,
            bos_token => token_piece(model, model.token_bos()),
            eos_token => token_piece(model, model.token_eos()),
            enable_thinking => enable_thinking,
        })
        .map_err(|e| LLMError::ProviderError(format!("Failed to render chat template: {e}")))?;

    let starts_in_thinking = prompt_starts_in_thinking(&prompt);

    let prompt_tail_len = 1200.min(prompt.len());
    let prompt_tail = if prompt_tail_len > 0 {
        &prompt[prompt.len() - prompt_tail_len..]
    } else {
        ""
    };
    let tools_in_prompt = tools.is_some() && prompt.contains("tools");
    log::debug!(
        "render_template: prompt_len={}, starts_in_thinking={}, tools_section_in_prompt={}, prompt_tail=<<<{}>>>",
        prompt.len(),
        starts_in_thinking,
        tools_in_prompt,
        prompt_tail
    );

    Ok(ChatTemplateResult {
        prompt,
        grammar,
        preserved_tokens: known_preserved_tokens(),
        additional_stops: known_stop_sequences(),
        starts_in_thinking,
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
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn known_stop_sequences() -> Vec<String> {
    Vec::new()
}
