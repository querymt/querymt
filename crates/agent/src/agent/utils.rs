//! Utility functions for the agent

use crate::acp::protocol::{ContentBlock, EmbeddedResourceResource, ToolCallLocation, ToolKind};
use log::warn;

/// Trace-only mapping pinned to semantic-conventions-genai e07f4ebacb08f56db8c4c882d117720333fbca04.
pub(crate) mod genai {
    use opentelemetry::{
        Array, Value,
        trace::{Status, TraceContextExt},
    };
    use querymt::chat::{ChatMessage, ChatOutput, ChatOutputStatus, ChatRole, FinishReason};
    use querymt::error::LLMError;
    use tracing::Span;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    use crate::model::{AgentMessage, MessagePart};

    pub(crate) fn agent_id(span: &Span, id: Option<&str>) {
        if let Some(id) = id.filter(|id| !id.trim().is_empty()) {
            span.set_attribute("gen_ai.agent.id", id.to_owned());
        }
    }

    tokio::task_local! {
        // Only the built-in invocation future carries the existing semantic tool span.
        static TOOL_SPAN: Span;
    }

    pub(crate) async fn tool_scope<T>(
        span: Span,
        future: impl std::future::Future<Output = T>,
    ) -> T {
        TOOL_SPAN.scope(span, future).await
    }

    fn rename_tool(span: &Span, name: String) {
        span.record("otel.name", &name);
        // Updating the tracing field alone cannot rename the started OTel span in 0.33.
        span.context().span().update_name(name);
    }

    pub(crate) fn skill_name(name: &str) {
        let _ = TOOL_SPAN.try_with(|span| {
            span.set_attribute("gen_ai.skill.name", name.to_owned());
            rename_tool(span, format!("execute_tool skill {name}"));
        });
    }

    pub(crate) fn shell_executable(name: &str) {
        let _ = TOOL_SPAN.try_with(|span| {
            span.set_attribute("process.executable.name", name.to_owned());
            rename_tool(span, format!("execute_tool shell {name}"));
        });
    }

    pub(crate) fn shell_exit_code(code: Option<i32>) {
        if let Some(code) = code {
            let _ = TOOL_SPAN.try_with(|span| {
                span.set_attribute("process.exit.code", i64::from(code));
            });
        }
    }

    /// Keep only typed successful summaries, projected alongside the effective history.
    /// Canonical assistant output supersedes legacy parts, including compaction parts.
    pub(crate) fn compaction_summaries(
        history: &[AgentMessage],
        projected: &[ChatMessage],
    ) -> Vec<ChatMessage> {
        history
            .iter()
            .zip(projected)
            .filter(|(message, _)| {
                message.parts.iter().any(|part| {
                    matches!(part, MessagePart::Compaction { summary, .. } if !summary.trim().is_empty())
                }) && !(message.role == ChatRole::Assistant
                    && message.parts.iter().any(|part| matches!(part, MessagePart::Output { .. })))
            })
            .map(|(_, projected)| projected.clone())
            .collect()
    }

    pub(crate) fn conversation_compacted(
        span: &Span,
        summaries: &[ChatMessage],
        messages: &[ChatMessage],
    ) {
        // Hooks/middleware can replace the final request; cache hints are not content.
        if summaries.iter().any(|summary| {
            messages.iter().any(|message| {
                message.role == summary.role && message.payload() == summary.payload()
            })
        }) {
            span.set_attribute("gen_ai.conversation.compacted", true);
        }
    }

    /// First received chunk, not first visible token. State belongs to the logical chat,
    /// so request setup and subsequent retry waits count, but provider construction does not.
    #[derive(Default)]
    pub(crate) struct FirstChunk {
        started: Option<std::time::Instant>,
        recorded: bool,
    }

    impl FirstChunk {
        pub(crate) fn start(&mut self) {
            if !self.recorded && self.started.is_none() {
                self.started = Some(std::time::Instant::now());
            }
        }

        pub(crate) fn received(&mut self, span: &Span) {
            if let Some(started) = self.started.take() {
                span.set_attribute(
                    "gen_ai.response.time_to_first_chunk",
                    started.elapsed().as_secs_f64(),
                );
                self.recorded = true;
            }
        }
    }

    /// Only scalar request settings are retained, never configuration or credentials.
    #[derive(Clone, Copy, Debug, Default)]
    pub(crate) struct GenerationSettings {
        max_tokens: Option<i64>,
        temperature: Option<f64>,
        top_p: Option<f64>,
        reasoning: Option<&'static str>,
    }

    impl GenerationSettings {
        pub(crate) fn from_config(provider: &str, config: &serde_json::Value) -> Self {
            if !matches!(provider, "openai" | "codex" | "anthropic")
                || config.get("extra_body").is_some_and(|value| {
                    // These known storage/cache/display fields cannot override the four knobs.
                    !value.is_null()
                        && value.as_object().is_none_or(|map| {
                            map.keys().any(|key| {
                                !matches!(
                                    key.as_str(),
                                    "store" | "promptCacheKey" | "prompt_cache_key" | "verbosity"
                                )
                            })
                        })
                })
            {
                return Self::default();
            }
            let number = |key: &str, upper: f64| {
                let value = config.get(key)?.as_f64()?;
                // These implementations deserialize to f32 before serializing requests.
                (value.is_finite() && (0.0..=upper).contains(&value))
                    .then_some(f64::from(value as f32))
            };
            let effort = config.get("reasoning_effort").and_then(|value| {
                serde_json::from_value::<querymt::chat::ReasoningEffort>(value.clone()).ok()
            });
            Self {
                max_tokens: if provider == "codex" {
                    None // The Codex request deliberately ignores this config field.
                } else {
                    config
                        .get("max_tokens")
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|value| u32::try_from(value).ok())
                        .map(i64::from)
                },
                temperature: if provider == "anthropic" && effort.is_some() {
                    Some(1.0)
                } else {
                    number("temperature", 2.0)
                },
                top_p: number("top_p", 1.0),
                reasoning: if provider == "anthropic" {
                    None // Anthropic sends a thinking mode/budget, not an effort level.
                } else {
                    effort.map(|effort| match effort {
                        querymt::chat::ReasoningEffort::Low => "low",
                        querymt::chat::ReasoningEffort::Medium => "medium",
                        querymt::chat::ReasoningEffort::High => "high",
                        querymt::chat::ReasoningEffort::Max => "xhigh",
                    })
                },
            }
        }

        pub(crate) fn record(&self, span: &Span) {
            if let Some(value) = self.max_tokens {
                span.set_attribute("gen_ai.request.max_tokens", value);
            }
            if let Some(value) = self.temperature {
                span.set_attribute("gen_ai.request.temperature", value);
            }
            if let Some(value) = self.top_p {
                span.set_attribute("gen_ai.request.top_p", value);
            }
            if let Some(value) = self.reasoning {
                span.set_attribute("gen_ai.request.reasoning.level", value);
            }
        }
    }

    pub(crate) fn provider_name(provider: &str) -> &str {
        match provider {
            "codex" => "openai",
            "google" => "gcp.gemini",
            "mistral" => "mistral_ai",
            "xai" => "x_ai",
            "moonshotai" => "moonshot_ai",
            _ => provider,
        }
    }

    pub(crate) fn finish_reason(reason: &str) {
        Span::current().set_attribute(
            "gen_ai.response.finish_reasons",
            Value::Array(Array::String(vec![reason.to_owned().into()])),
        );
    }

    pub(crate) fn error(error_type: &'static str) {
        let span = Span::current();
        span.set_attribute("error.type", error_type);
        span.set_status(Status::error(""));
    }

    pub(crate) fn llm_error(error: &LLMError) {
        let error_type = match error {
            LLMError::Cancelled => return,
            LLMError::AuthError(_) => "authentication",
            LLMError::RateLimited { .. } => "rate_limited",
            LLMError::InvalidRequest(_) => "invalid_request",
            LLMError::ResponseFormatError { .. } | LLMError::JsonError(_) => "response_format",
            LLMError::NotImplemented(_) => "unsupported_operation",
            LLMError::Transport { .. } | LLMError::HttpError(_) | LLMError::IoError(_) => {
                "transport"
            }
            _ => "provider_error",
        };
        self::error(error_type);
    }

    pub(crate) fn output(output: &ChatOutput) {
        let span = Span::current();
        // Built-in response provenance includes the server-reported model when available.
        // External providers may omit it; never replace the requested model or span name.
        if let Some(origin) = &output.provenance {
            if !origin.provider.is_empty() {
                span.set_attribute(
                    "gen_ai.provider.name",
                    provider_name(&origin.provider).to_owned(),
                );
            }
            if !origin.model.is_empty() {
                span.set_attribute("gen_ai.response.model", origin.model.clone());
            }
        }
        if let Some(id) = &output.response_id {
            span.set_attribute("gen_ai.response.id", id.clone());
        }
        if let Some(usage) = &output.usage {
            // Built-in adapters use exclusive buckets (OpenAI/Codex subtract cache/reasoning;
            // Anthropic reports cache separately; other adapters leave extra buckets zero).
            span.set_attribute(
                "gen_ai.usage.input_tokens",
                i64::from(usage.input_tokens)
                    + i64::from(usage.cache_read)
                    + i64::from(usage.cache_write),
            );
            span.set_attribute(
                "gen_ai.usage.output_tokens",
                i64::from(usage.output_tokens) + i64::from(usage.reasoning_tokens),
            );
            span.set_attribute(
                "gen_ai.usage.cache_read.input_tokens",
                i64::from(usage.cache_read),
            );
            span.set_attribute(
                "gen_ai.usage.cache_write.input_tokens",
                i64::from(usage.cache_write),
            );
            span.set_attribute(
                "gen_ai.usage.reasoning.output_tokens",
                i64::from(usage.reasoning_tokens),
            );
        }
        let reason = match output.finish_reason {
            Some(FinishReason::Stop) => "stop",
            Some(FinishReason::Length) => "length",
            Some(FinishReason::ContentFilter) => "content_filter",
            Some(FinishReason::ToolCalls) => "tool_calls",
            Some(FinishReason::Other) => "other",
            Some(FinishReason::Unknown) => "unknown",
            None if output.status == Some(ChatOutputStatus::Completed) => "unknown",
            Some(FinishReason::Error) | None => "error",
        };
        finish_reason(reason);
        if output.status == Some(ChatOutputStatus::Failed)
            || output.finish_reason == Some(FinishReason::Error)
        {
            error("provider_error");
        }
    }
}

const ATTACHMENTS_DISPLAY_FALLBACK: &str = "(attachments included)";
const IMAGE_DISPLAY_FALLBACK: &str = "(image attached)";

/// Collect user-authored text without incorporating attachment payloads.
pub fn format_prompt_user_text_only(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Render prompt blocks for user-facing displays/events.
///
/// If there is no user text but attachments exist, return a placeholder.
pub fn render_prompt_for_display(blocks: &[ContentBlock]) -> String {
    let user_text = format_prompt_user_text_only(blocks).trim().to_string();
    if !user_text.is_empty() {
        return user_text;
    }

    if crate::model::prompt_contains_images(blocks) {
        IMAGE_DISPLAY_FALLBACK.to_string()
    } else if !blocks.is_empty() {
        ATTACHMENTS_DISPLAY_FALLBACK.to_string()
    } else {
        String::new()
    }
}

/// Render prompt blocks for LLM context/history replay.
pub fn render_prompt_for_llm(blocks: &[ContentBlock], max_prompt_bytes: Option<usize>) -> String {
    let mut content = String::new();
    for block in blocks {
        if !content.is_empty() {
            content.push_str("\n\n");
        }
        match block {
            ContentBlock::Text(text) => {
                content.push_str(&text.text);
            }
            ContentBlock::ResourceLink(link) => {
                content.push_str(&format!(
                    "[Resource: {}] {}\n{}",
                    link.name,
                    link.uri,
                    link.description.clone().unwrap_or_default()
                ));
            }
            ContentBlock::Resource(resource) => match &resource.resource {
                EmbeddedResourceResource::TextResourceContents(text) => {
                    content.push_str(&format!("[Embedded Resource: {}]\n{}", text.uri, text.text));
                }
                EmbeddedResourceResource::BlobResourceContents(blob) => {
                    content.push_str(&format!(
                        "[Embedded Resource: {}] ({})",
                        blob.uri,
                        blob.mime_type.as_deref().unwrap_or("binary")
                    ));
                }
                _ => {
                    content.push_str("[Embedded Resource: unsupported]");
                }
            },
            ContentBlock::Image(image) => {
                content.push_str(&format!("[Image: {}]", image.mime_type));
            }
            ContentBlock::Audio(audio) => {
                content.push_str(&format!("[Audio: {}]", audio.mime_type));
            }
            _ => {
                content.push_str("[Unsupported content block]");
            }
        }
    }

    if let Some(max_bytes) = max_prompt_bytes {
        truncate_to_bytes(&content, max_bytes)
    } else {
        content
    }
}

/// Approximates token count based on character count
pub fn approximate_token_count(messages: &[querymt::chat::ChatMessage]) -> usize {
    let mut chars = 0usize;
    for msg in messages {
        chars += msg.portable_input_parts().len() + msg.text().len();
    }
    (chars / 4).max(1)
}

/// Truncates a string to fit within a byte limit
pub fn truncate_to_bytes(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let note = "\n[truncated]";
    if max_bytes <= note.len() {
        // Not enough room for text + note: return as many whole chars as fit.
        return input.chars().take(max_bytes).collect();
    }

    let mut end = max_bytes - note.len();
    while !input.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    let mut truncated = input[..end].to_string();
    truncated.push_str(note);
    truncated
}

/// Determines the tool kind for a given tool name
pub fn tool_kind_for_tool(name: &str) -> ToolKind {
    match name {
        "search_text" => ToolKind::Search,
        "mdq" => ToolKind::Search,
        "write_file" | "edit" | "multiedit" => ToolKind::Edit,
        "delete_file" => ToolKind::Delete,
        "shell" => ToolKind::Execute,
        "web_fetch" | "browse" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// Extracts file paths from tool arguments for location tracking
pub fn extract_locations(args: &serde_json::Value) -> Vec<ToolCallLocation> {
    let mut locations = Vec::new();
    let Some(map) = args.as_object() else {
        return locations;
    };
    if let Some(path) = map.get("path").and_then(|v| v.as_str()) {
        locations.push(ToolCallLocation::new(path));
    }
    if let Some(root) = map.get("root").and_then(|v| v.as_str()) {
        locations.push(ToolCallLocation::new(root));
    }
    locations
}

/// Convert usize to u32 for wire-facing payloads, logging and clamping on overflow.
pub fn u32_from_usize(value: usize, field_name: &str, session_id: Option<&str>) -> u32 {
    match u32::try_from(value) {
        Ok(v) => v,
        Err(_) => {
            if let Some(session_id) = session_id {
                warn!(
                    "Session {}: {} overflowed u32 ({}), clamping to {}",
                    session_id,
                    field_name,
                    value,
                    u32::MAX
                );
            } else {
                warn!(
                    "{} overflowed u32 ({}), clamping to {}",
                    field_name,
                    value,
                    u32::MAX
                );
            }
            u32::MAX
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{format_prompt_user_text_only, render_prompt_for_display};
    use crate::acp::protocol::{ContentBlock, TextContent};

    #[tokio::test]
    async fn genai_generation_settings_are_typed_bounded_and_provider_specific() {
        use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
        use opentelemetry::Value;
        for provider in ["openai", "codex", "anthropic", "google", "xai", "unknown"] {
            for effort in [
                None,
                Some("low"),
                Some("medium"),
                Some("high"),
                Some("max"),
                Some("SECRET_ARGUMENT"),
            ] {
                let config = serde_json::json!({"max_tokens":u32::MAX, "temperature":0.5, "top_p":0.75, "reasoning_effort":effort,
                    "extra_body":{"store":false, "promptCacheKey":"SECRET_ARGUMENT", "verbosity":"SECRET_RESPONSE"}});
                let (_, spans) = capture(async {
                    let span = tracing::info_span!("settings");
                    span.in_scope(|| {
                        super::genai::GenerationSettings::from_config(provider, &config)
                            .record(&span)
                    });
                })
                .await;
                let span = &spans[0];
                let known = matches!(provider, "openai" | "codex" | "anthropic");
                let reasoning = match effort {
                    Some("max") => Some("xhigh"),
                    Some("low" | "medium" | "high") => effort,
                    _ => None,
                };
                assert_eq!(
                    attr(span, "gen_ai.request.max_tokens"),
                    (known && provider != "codex").then_some(&Value::I64(i64::from(u32::MAX)))
                );
                assert_eq!(
                    attr(span, "gen_ai.request.temperature"),
                    known.then_some(&Value::F64(
                        if provider == "anthropic" && reasoning.is_some() {
                            1.0
                        } else {
                            0.5
                        }
                    ))
                );
                assert_eq!(
                    attr(span, "gen_ai.request.top_p"),
                    known.then_some(&Value::F64(0.75))
                );
                assert_eq!(
                    attr(span, "gen_ai.request.reasoning.level"),
                    reasoning
                        .filter(|_| known && provider != "anthropic")
                        .map(Value::from)
                        .as_ref()
                );
                assert_private(&spans);
            }
        }
        for config in [
            serde_json::json!({}),
            serde_json::json!({"max_tokens":-1, "temperature":"1", "top_p":2, "reasoning_effort":"SECRET_ARGUMENT"}),
            serde_json::json!({"max_tokens":u64::MAX, "temperature":-0.5, "top_p":-1}),
            serde_json::json!({"max_tokens":12, "temperature":0.5, "top_p":0.75, "reasoning_effort":"high", "extra_body":{"reasoning":{"effort":"SECRET_ARGUMENT"}}}),
        ] {
            let (_, spans) = capture(async {
                let span = tracing::info_span!("settings");
                span.in_scope(|| {
                    super::genai::GenerationSettings::from_config("openai", &config).record(&span)
                });
            })
            .await;
            assert!(
                spans[0]
                    .attributes
                    .iter()
                    .all(|attr| !attr.key.as_str().starts_with("gen_ai.request."))
            );
            assert_private(&spans);
        }
    }

    #[tokio::test]
    async fn genai_tool_scope_is_future_local_and_does_not_survive_drop() {
        use crate::test_utils::helpers::genai_trace::{attr, capture};
        use opentelemetry::Value;
        let (_, spans) = capture(async {
            let first = tracing::info_span!("first", otel.name = tracing::field::Empty);
            let second = tracing::info_span!("second", otel.name = tracing::field::Empty);
            tokio::join!(
                super::genai::tool_scope(first, async {
                    tokio::task::yield_now().await;
                    super::genai::skill_name("one");
                }),
                super::genai::tool_scope(second, async {
                    tokio::task::yield_now().await;
                    super::genai::skill_name("two");
                }),
            );
            let span = tracing::info_span!("dropped", otel.name = tracing::field::Empty);
            let mut future = Box::pin(super::genai::tool_scope(span, async {
                std::future::pending::<()>().await
            }));
            assert!(futures_util::poll!(&mut future).is_pending());
            drop(future);
            super::genai::skill_name("must-not-leak");
        })
        .await;
        assert_eq!(
            attr(
                spans
                    .iter()
                    .find(|span| span.name == "execute_tool skill one")
                    .unwrap(),
                "gen_ai.skill.name"
            ),
            Some(&Value::from("one"))
        );
        assert_eq!(
            attr(
                spans
                    .iter()
                    .find(|span| span.name == "execute_tool skill two")
                    .unwrap(),
                "gen_ai.skill.name"
            ),
            Some(&Value::from("two"))
        );
        assert!(
            attr(
                spans.iter().find(|span| span.name == "dropped").unwrap(),
                "gen_ai.skill.name"
            )
            .is_none()
        );
    }

    #[test]
    fn genai_provider_names_use_actual_provider_ids() {
        for (provider, expected) in [
            ("codex", "openai"),
            ("google", "gcp.gemini"),
            ("mistral", "mistral_ai"),
            ("xai", "x_ai"),
            ("moonshotai", "moonshot_ai"),
            ("external", "external"),
        ] {
            assert_eq!(super::genai::provider_name(provider), expected);
        }
    }

    #[tokio::test]
    async fn genai_missing_finish_reason_is_unknown_only_for_completed_output() {
        use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
        use opentelemetry::{Array, Value, trace::Status};
        use querymt::chat::{ChatOutput, ChatOutputStatus, FinishReason};
        use tracing::Instrument;

        for (status, finish, reason, failed) in [
            (Some(ChatOutputStatus::Completed), None, "unknown", false),
            (Some(ChatOutputStatus::Failed), None, "error", true),
            (Some(ChatOutputStatus::InProgress), None, "error", false),
            (Some(ChatOutputStatus::Incomplete), None, "error", false),
            (None, None, "error", false),
            (
                Some(ChatOutputStatus::Completed),
                Some(FinishReason::Error),
                "error",
                true,
            ),
        ] {
            let mut output = ChatOutput::from_projections(
                None,
                Some("SECRET_RESPONSE".into()),
                None,
                None,
                finish,
            );
            output.status = status;
            let (_, spans) = capture(async {
                async {
                    super::genai::output(&output);
                }
                .instrument(tracing::info_span!("test-chat"))
                .await
            })
            .await;
            assert_eq!(spans.len(), 1);
            assert_eq!(
                attr(&spans[0], "gen_ai.response.finish_reasons"),
                Some(&Value::Array(Array::String(vec![reason.into()])))
            );
            assert_eq!(
                spans[0].status,
                if failed {
                    Status::error("")
                } else {
                    Status::Unset
                }
            );
            assert_private(&spans);
        }
    }

    #[test]
    fn render_prompt_for_display_prefers_user_text() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("hello".to_string())),
            ContentBlock::Text(TextContent::new("[file: x]".to_string())),
        ];

        assert_eq!(render_prompt_for_display(&blocks), "hello\n\n[file: x]");
        assert_eq!(format_prompt_user_text_only(&blocks), "hello\n\n[file: x]");
    }

    #[test]
    fn render_prompt_for_display_uses_attachment_placeholder() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("".to_string())),
            ContentBlock::Text(TextContent::new("[file: x]".to_string())),
        ];

        assert_eq!(render_prompt_for_display(&blocks), "[file: x]");
    }
}
