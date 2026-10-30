//! Utility functions for the agent

use crate::acp::protocol::{ContentBlock, EmbeddedResourceResource, ToolCallLocation, ToolKind};
use log::warn;

/// Latency histograms for the model call and the work that precedes it.
///
/// Instruments are built per call from the global meter, like the tool counters in
/// [`genai`]: the SDK deduplicates them, and it keeps tests that install a capture
/// provider after first use correct.
pub(crate) mod llm_metrics {
    use opentelemetry::KeyValue;
    use opentelemetry::metrics::Histogram;
    use std::time::Instant;
    use tokio_util::sync::CancellationToken;

    /// OTel GenAI advisory buckets, extended for long agentic generations.
    const DURATION_BOUNDS: &[f64] = &[
        0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92,
        163.84, 327.68, 655.36, 1310.72,
    ];
    const FIRST_RESPONSE_BOUNDS: &[f64] =
        &[0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0];
    const OVERHEAD_BOUNDS: &[f64] = &[
        0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
    ];

    fn histogram(name: &'static str, description: &'static str, bounds: &[f64]) -> Histogram<f64> {
        opentelemetry::global::meter("querymt-agent")
            .f64_histogram(name)
            .with_unit("s")
            .with_description(description)
            .with_boundaries(bounds.to_vec())
            .build()
    }

    fn model_attrs(provider: &str, model: &str) -> [KeyValue; 3] {
        [
            KeyValue::new("gen_ai.operation.name", "chat"),
            KeyValue::new("gen_ai.provider.name", provider.to_owned()),
            KeyValue::new("gen_ai.request.model", model.to_owned()),
        ]
    }

    /// Records `gen_ai.client.operation.duration` when dropped, so every exit path of the
    /// model call (success, error, cancellation, dropped future) is measured once.
    pub(crate) struct ChatTimer {
        started: Instant,
        provider: String,
        model: String,
        cancel: CancellationToken,
        ok: bool,
    }

    impl ChatTimer {
        pub(crate) fn start(provider: &str, model: &str, cancel: &CancellationToken) -> Self {
            Self {
                started: Instant::now(),
                provider: provider.to_owned(),
                model: model.to_owned(),
                cancel: cancel.clone(),
                ok: false,
            }
        }

        /// Record the request as successful now; consuming the timer stops the clock here.
        pub(crate) fn succeeded(mut self) {
            self.ok = true;
        }
    }

    impl Drop for ChatTimer {
        fn drop(&mut self) {
            let mut attrs = model_attrs(&self.provider, &self.model).to_vec();
            if !self.ok {
                let kind = if self.cancel.is_cancelled() {
                    "cancelled"
                } else {
                    "_OTHER"
                };
                attrs.push(KeyValue::new("error.type", kind));
            }
            histogram(
                "gen_ai.client.operation.duration",
                "Duration of one logical model request",
                DURATION_BOUNDS,
            )
            .record(self.started.elapsed().as_secs_f64(), &attrs);
        }
    }

    /// First-byte and first-token latency of one streamed request. Absent values are skipped.
    pub(crate) fn stream_latency(
        provider: &str,
        model: &str,
        first_byte: Option<f64>,
        first_token: Option<f64>,
    ) {
        let attrs = model_attrs(provider, model);
        if let Some(secs) = first_byte {
            histogram(
                "qmt.gen_ai.client.time_to_first_byte",
                "Time from request dispatch to response headers",
                FIRST_RESPONSE_BOUNDS,
            )
            .record(secs, &attrs);
        }
        if let Some(secs) = first_token {
            histogram(
                "qmt.gen_ai.client.time_to_first_token",
                "Time from request dispatch to the first chunk carrying model output",
                FIRST_RESPONSE_BOUNDS,
            )
            .record(secs, &attrs);
        }
    }

    /// Agent-side time in a step outside the model call. Only the pre-request phase
    /// (everything between starting a step and dispatching the request) is measured.
    pub(crate) fn step_overhead(phase: &'static str, secs: f64, provider: &str, model: &str) {
        let mut attrs = model_attrs(provider, model).to_vec();
        attrs.push(KeyValue::new("phase", phase));
        histogram(
            "qmt.agent.step.overhead",
            "Agent time in a step outside the model call",
            OVERHEAD_BOUNDS,
        )
        .record(secs, &attrs);
    }
}

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

    /// Whether a stream chunk carries actual model output (a token) rather than framing,
    /// metadata or usage. The first such chunk defines time-to-first-token.
    ///
    /// Deliberately stricter than the retry-safety predicate in `llm_retry`: a retry must
    /// treat any structured event as committed, but `message_start`-style metadata and
    /// empty deltas are not tokens, so counting them would collapse TTFT into
    /// time-to-first-chunk.
    pub(crate) fn chunk_commits_output(chunk: &querymt::chat::StreamChunk) -> bool {
        use querymt::chat::{
            ChatMessagePartDelta, ChatOutputItem, StreamChunk, StructuredStreamEvent,
        };
        match chunk {
            StreamChunk::Structured(event) => match event {
                StructuredStreamEvent::MessagePartDelta { delta, .. } => match delta {
                    ChatMessagePartDelta::Text { delta }
                    | ChatMessagePartDelta::Refusal { delta } => !delta.is_empty(),
                },
                StructuredStreamEvent::ReasoningPartDelta { delta, .. }
                | StructuredStreamEvent::FunctionArgumentsDelta { delta, .. } => !delta.is_empty(),
                // A function call announces itself before its arguments stream.
                StructuredStreamEvent::ItemStarted { item, .. } => {
                    matches!(item, ChatOutputItem::FunctionCall(_))
                }
                // Fallback for providers that deliver whole items without deltas.
                StructuredStreamEvent::ItemCompleted { .. } => true,
                StructuredStreamEvent::ResponseMetadata { .. }
                | StructuredStreamEvent::MessagePartStarted { .. }
                | StructuredStreamEvent::ReasoningPartStarted { .. }
                | StructuredStreamEvent::ResponseTerminal { .. } => false,
            },
            StreamChunk::Text(text) | StreamChunk::Thinking(text) => !text.is_empty(),
            StreamChunk::ToolUseStart { .. } | StreamChunk::ToolUseComplete { .. } => true,
            StreamChunk::ToolUseInputDelta { partial_json, .. } => !partial_json.is_empty(),
            StreamChunk::ThinkingSignature(_)
            | StreamChunk::RedactedThinking(_)
            | StreamChunk::Usage(_)
            | StreamChunk::Done { .. } => false,
        }
    }

    /// Timing of one logical chat stream, recorded on the chat span.
    ///
    /// State belongs to the logical chat, so request setup and subsequent retry waits
    /// count, but provider construction does not. Recorded attributes (seconds unless noted):
    ///
    /// - `llm.time_to_first_byte`: `chat_stream()` returned (response headers received).
    /// - `gen_ai.response.time_to_first_chunk`: first received chunk of any kind.
    /// - `llm.time_to_first_token`: first chunk carrying model output. This is the name
    ///   Otelite reads for TTFT; it is client-side, so it is not `gen_ai.server.*`.
    /// - `qmt.stream.{chunk_count, wait_ms, process_ms, max_gap_ms}`: aggregates only, so
    ///   a slow network is distinguishable from slow local chunk handling.
    #[derive(Default)]
    pub(crate) struct StreamTiming {
        started: Option<std::time::Instant>,
        first_byte: Option<f64>,
        first_chunk: Option<f64>,
        first_token: Option<f64>,
        chunks: u64,
        last_chunk: Option<std::time::Instant>,
        max_gap: std::time::Duration,
        wait: std::time::Duration,
        process: std::time::Duration,
        poll_started: Option<std::time::Instant>,
        poll_ended: Option<std::time::Instant>,
    }

    impl StreamTiming {
        /// Call before each physical request attempt. Only the first call starts the clock.
        pub(crate) fn start(&mut self) {
            // Gaps and local-processing time never span attempts.
            self.last_chunk = None;
            self.poll_ended = None;
            if self.started.is_none() {
                self.started = Some(std::time::Instant::now());
            }
        }

        pub(crate) fn headers_received(&mut self, span: &Span) {
            if let (Some(started), None) = (self.started, self.first_byte) {
                let secs = started.elapsed().as_secs_f64();
                span.set_attribute("llm.time_to_first_byte", secs);
                self.first_byte = Some(secs);
            }
        }

        /// `content` is [`chunk_commits_output`] for the chunk.
        pub(crate) fn chunk_received(&mut self, span: &Span, content: bool) {
            let now = std::time::Instant::now();
            if let Some(started) = self.started {
                let secs = now.duration_since(started).as_secs_f64();
                if self.first_chunk.is_none() {
                    span.set_attribute("gen_ai.response.time_to_first_chunk", secs);
                    self.first_chunk = Some(secs);
                }
                if content && self.first_token.is_none() {
                    span.set_attribute("llm.time_to_first_token", secs);
                    self.first_token = Some(secs);
                }
            }
            if let Some(previous) = self.last_chunk {
                self.max_gap = self.max_gap.max(now.duration_since(previous));
            }
            self.last_chunk = Some(now);
            self.chunks += 1;
        }

        /// Bracket each await on the next stream chunk; the time between brackets is
        /// network wait, the time between `wait_end` and the next `wait_begin` is local work.
        pub(crate) fn wait_begin(&mut self) {
            let now = std::time::Instant::now();
            if let Some(ended) = self.poll_ended.take() {
                self.process += now.duration_since(ended);
            }
            self.poll_started = Some(now);
        }

        pub(crate) fn wait_end(&mut self) {
            let now = std::time::Instant::now();
            if let Some(started) = self.poll_started.take() {
                self.wait += now.duration_since(started);
            }
            self.poll_ended = Some(now);
        }

        /// Record the stream aggregates once the stream is complete.
        pub(crate) fn finish(&self, span: &Span) {
            if self.chunks == 0 {
                return;
            }
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
            span.set_attribute("qmt.stream.chunk_count", self.chunks as i64);
            span.set_attribute("qmt.stream.wait_ms", ms(self.wait));
            span.set_attribute("qmt.stream.process_ms", ms(self.process));
            span.set_attribute("qmt.stream.max_gap_ms", ms(self.max_gap));
        }

        pub(crate) fn first_byte_secs(&self) -> Option<f64> {
            self.first_byte
        }

        pub(crate) fn first_token_secs(&self) -> Option<f64> {
            self.first_token
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
            if !matches!(
                provider,
                "openai" | "codex" | "anthropic" | "xai" | "google"
            ) || config.get("extra_body").is_some_and(|value| {
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
            }) {
                return Self::default();
            }
            let effort = config.get("reasoning_effort").and_then(|value| {
                serde_json::from_value::<querymt::chat::ReasoningEffort>(value.clone()).ok()
            });
            let level = effort.map(|effort| match effort {
                querymt::chat::ReasoningEffort::Low => "low",
                querymt::chat::ReasoningEffort::Medium => "medium",
                querymt::chat::ReasoningEffort::High => "high",
                querymt::chat::ReasoningEffort::Max => "xhigh",
            });
            if matches!(provider, "xai" | "google") {
                let model = config.get("model").and_then(serde_json::Value::as_str);
                let reasoning = model
                    .filter(|model| !model.trim().is_empty())
                    .and_then(|model| {
                        if provider == "xai" {
                            // Match xAI's Responses gate even on custom Chat Completions routes.
                            let name = model.trim().to_ascii_lowercase();
                            let name = name.rsplit('/').next().unwrap_or(name.as_str());
                            level.filter(|_| {
                                ["grok-3-mini", "grok-4.20-multi-agent", "grok-4.3"]
                                    .iter()
                                    .any(|prefix| name.starts_with(prefix))
                            })
                        } else if model.contains("2.5")
                            || config
                                .get("thinking_budget")
                                .is_some_and(|value| !value.is_null())
                        {
                            None // Google sends a budget instead of a level in these cases.
                        } else {
                            effort.map(|effort| match effort {
                                querymt::chat::ReasoningEffort::Low => "low",
                                querymt::chat::ReasoningEffort::Medium
                                | querymt::chat::ReasoningEffort::High
                                | querymt::chat::ReasoningEffort::Max => "high",
                            })
                        }
                    });
                return Self {
                    reasoning,
                    ..Self::default()
                };
            }
            let number = |key: &str, upper: f64| {
                let value = config.get(key)?.as_f64()?;
                // These implementations deserialize to f32 before serializing requests.
                (value.is_finite() && (0.0..=upper).contains(&value))
                    .then_some(f64::from(value as f32))
            };
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
                    level
                },
            }
        }

        /// Effective settings: the configured values with a request's applied
        /// overrides on top. A reasoning override only replaces a level the
        /// provider already reports, since providers such as Anthropic send a
        /// thinking mode/budget instead of a level.
        pub(crate) fn with_overrides(mut self, applied: querymt::chat::GenerationOptions) -> Self {
            if let Some(max_tokens) = applied.max_tokens {
                self.max_tokens = Some(i64::from(max_tokens));
            }
            if let (Some(effort), Some(_)) = (applied.reasoning_effort, self.reasoning) {
                self.reasoning = Some(match effort {
                    querymt::chat::ReasoningEffort::Low => "low",
                    querymt::chat::ReasoningEffort::Medium => "medium",
                    querymt::chat::ReasoningEffort::High => "high",
                    querymt::chat::ReasoningEffort::Max => "xhigh",
                });
            }
            self
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

    fn tool_meter() -> opentelemetry::metrics::Meter {
        opentelemetry::global::meter("querymt-agent")
    }

    /// Record the argument repairs that were actually applied to one call.
    ///
    /// Increments `querymt.tool.args_repaired{tool, rule}` once per applied
    /// mutation and lists the applied rules on `span` as
    /// `querymt.tool.args_repairs`. Called once per prepared call, never for
    /// scheduling inspections, so unchanged re-normalisation is not counted.
    pub(crate) fn args_repaired(span: &Span, tool: &str, rules: &[&'static str]) {
        if rules.is_empty() {
            return;
        }
        let counter = tool_meter()
            .u64_counter("querymt.tool.args_repaired")
            .with_description("Tool argument repairs applied before validation")
            .build();
        for rule in rules {
            counter.add(
                1,
                &[
                    opentelemetry::KeyValue::new("tool", tool.to_owned()),
                    opentelemetry::KeyValue::new("rule", *rule),
                ],
            );
        }
        span.set_attribute(
            "querymt.tool.args_repairs",
            Value::Array(Array::String(
                rules.iter().map(|rule| (*rule).to_owned().into()).collect(),
            )),
        );
    }

    /// Record the final outcome of one call, independent of per-rule repair
    /// counts: `querymt.tool.calls{tool, repaired, outcome}`.
    ///
    /// `outcome` is `success` (the tool ran and succeeded), `error` (the tool
    /// ran or its pipeline failed) or `rejected` (never ran: validation
    /// failure, hook block or middleware rejection). Querying
    /// `repaired=true` by outcome separates repaired calls that succeed from
    /// repaired calls that still fail.
    pub(crate) fn tool_call_outcome(tool: &str, repaired: bool, outcome: &'static str) {
        tool_meter()
            .u64_counter("querymt.tool.calls")
            .with_description("Tool calls by repaired indicator and final outcome")
            .build()
            .add(
                1,
                &[
                    opentelemetry::KeyValue::new("tool", tool.to_owned()),
                    opentelemetry::KeyValue::new("repaired", repaired),
                    opentelemetry::KeyValue::new("outcome", outcome),
                ],
            );
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
            let reasoning = i64::from(usage.reasoning_tokens);
            span.set_attribute("gen_ai.usage.reasoning.output_tokens", reasoning);
            // Collector compatibility alias; keep the normalized standard bucket unchanged.
            span.set_attribute("gen_ai.usage.reasoning_tokens", reasoning);
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
        "write_file" | "edit" | "multiedit" | "accept_delegation_result" => ToolKind::Edit,
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
    if let Some(file_path) = map.get("file_path").and_then(|v| v.as_str()) {
        locations.push(ToolCallLocation::new(file_path));
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
    use super::{extract_locations, format_prompt_user_text_only, render_prompt_for_display};
    use crate::acp::protocol::{ContentBlock, TextContent, ToolCallLocation};

    #[test]
    fn extract_locations_includes_file_path_path_and_root() {
        assert_eq!(
            extract_locations(&serde_json::json!({"file_path": "/a.rs", "old_string": "x"})),
            vec![ToolCallLocation::new("/a.rs")]
        );
        assert_eq!(
            extract_locations(&serde_json::json!({"path": "p", "root": "r"})),
            vec![ToolCallLocation::new("p"), ToolCallLocation::new("r")]
        );
        assert!(extract_locations(&serde_json::json!("not an object")).is_empty());
    }

    #[test]
    fn ttft_predicate_ignores_metadata_framing_and_empty_deltas() {
        use super::genai::chunk_commits_output;
        use querymt::chat::{
            ChatMessagePartDelta, ChatOutputStatus, FinishReason, StreamChunk,
            StructuredStreamEvent,
        };

        let metadata = StreamChunk::Structured(StructuredStreamEvent::ResponseMetadata {
            response_id: Some("r".into()),
            status: None,
            usage: None,
            finish_reason: None,
            provenance: None,
        });
        let terminal = StreamChunk::Structured(StructuredStreamEvent::ResponseTerminal {
            status: ChatOutputStatus::Completed,
            usage: None,
            finish_reason: Some(FinishReason::Stop),
            detail: None,
        });
        let text_delta = |delta: &str| {
            StreamChunk::Structured(StructuredStreamEvent::MessagePartDelta {
                output_index: 0,
                content_index: 0,
                delta: ChatMessagePartDelta::Text {
                    delta: delta.into(),
                },
            })
        };

        // Anthropic `message_start` arrives first and is not a token.
        assert!(!chunk_commits_output(&metadata));
        assert!(!chunk_commits_output(&terminal));
        assert!(!chunk_commits_output(&text_delta("")));
        assert!(chunk_commits_output(&text_delta("hi")));
        assert!(!chunk_commits_output(&StreamChunk::Text(String::new())));
        assert!(chunk_commits_output(&StreamChunk::Text("hi".into())));
        assert!(chunk_commits_output(&StreamChunk::Thinking("hm".into())));
        assert!(!chunk_commits_output(&StreamChunk::Usage(
            Default::default()
        )));
        assert!(!chunk_commits_output(&StreamChunk::Done {
            finish_reason: FinishReason::Stop,
        }));
    }

    #[tokio::test]
    async fn stream_timing_records_ordered_ttfb_chunk_and_token_once() {
        use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
        use opentelemetry::Value;
        use std::time::Duration;

        let (_, spans) = capture(async {
            let span = tracing::info_span!("chat");
            let mut timing = super::genai::StreamTiming::default();
            timing.start();
            tokio::time::sleep(Duration::from_millis(20)).await;
            timing.headers_received(&span);
            // Metadata first: counts as a chunk, not as a token.
            timing.wait_begin();
            tokio::time::sleep(Duration::from_millis(20)).await;
            timing.wait_end();
            timing.chunk_received(&span, false);
            timing.wait_begin();
            tokio::time::sleep(Duration::from_millis(20)).await;
            timing.wait_end();
            timing.chunk_received(&span, true);
            // Later chunks and a retry's start() must not re-record.
            timing.start();
            timing.headers_received(&span);
            timing.chunk_received(&span, true);
            timing.finish(&span);
        })
        .await;

        let chat = &spans[0];
        let secs = |key: &str| match attr(chat, key) {
            Some(Value::F64(v)) => *v,
            other => panic!("{key}: {other:?}"),
        };
        let (ttfb, first_chunk, ttft) = (
            secs("llm.time_to_first_byte"),
            secs("gen_ai.response.time_to_first_chunk"),
            secs("llm.time_to_first_token"),
        );
        assert!(ttfb >= 0.02, "ttfb={ttfb}");
        assert!(first_chunk > ttfb, "first chunk follows headers");
        assert!(ttft > first_chunk + 0.015, "token follows metadata chunk");
        for key in [
            "llm.time_to_first_byte",
            "gen_ai.response.time_to_first_chunk",
            "llm.time_to_first_token",
        ] {
            assert_eq!(
                chat.attributes
                    .iter()
                    .filter(|a| a.key.as_str() == key)
                    .count(),
                1,
                "{key} must be recorded once"
            );
        }
        assert_eq!(attr(chat, "qmt.stream.chunk_count"), Some(&Value::I64(3)));
        assert!(secs("qmt.stream.wait_ms") >= 35.0);
        assert!(secs("qmt.stream.max_gap_ms") >= 15.0);
        assert!(attr(chat, "gen_ai.server.time_to_first_token").is_none());
        assert_private(&spans);
    }

    #[tokio::test]
    async fn stream_timing_without_chunks_records_no_aggregates() {
        use crate::test_utils::helpers::genai_trace::{attr, capture};
        let (_, spans) = capture(async {
            let span = tracing::info_span!("chat");
            let mut timing = super::genai::StreamTiming::default();
            timing.start();
            timing.finish(&span);
        })
        .await;
        assert!(attr(&spans[0], "qmt.stream.chunk_count").is_none());
        assert!(attr(&spans[0], "llm.time_to_first_token").is_none());
    }

    #[tokio::test]
    async fn chat_timer_records_duration_with_outcome() {
        use crate::test_utils::helpers::genai_metrics;
        use tokio_util::sync::CancellationToken;
        genai_metrics::install();

        let token = CancellationToken::new();
        super::llm_metrics::ChatTimer::start("anthropic", "m-ok", &token).succeeded();
        drop(super::llm_metrics::ChatTimer::start(
            "anthropic",
            "m-err",
            &token,
        ));
        let cancelled = CancellationToken::new();
        let timer = super::llm_metrics::ChatTimer::start("anthropic", "m-cancel", &cancelled);
        cancelled.cancel();
        drop(timer);
        super::llm_metrics::stream_latency("anthropic", "m-ok", Some(0.5), Some(1.5));
        super::llm_metrics::step_overhead("pre_request", 0.05, "anthropic", "m-ok");

        let count = |name: &str, model: &str, error: Option<&str>| {
            genai_metrics::histogram_count(name, model, error)
        };
        assert_eq!(count("gen_ai.client.operation.duration", "m-ok", None), 1);
        assert_eq!(
            count("gen_ai.client.operation.duration", "m-err", Some("_OTHER")),
            1
        );
        assert_eq!(
            count(
                "gen_ai.client.operation.duration",
                "m-cancel",
                Some("cancelled")
            ),
            1
        );
        assert_eq!(
            count("qmt.gen_ai.client.time_to_first_byte", "m-ok", None),
            1
        );
        assert_eq!(
            count("qmt.gen_ai.client.time_to_first_token", "m-ok", None),
            1
        );
        assert_eq!(count("qmt.agent.step.overhead", "m-ok", None), 1);
    }

    #[tokio::test]
    async fn genai_generation_settings_are_typed_bounded_and_provider_specific() {
        use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
        use opentelemetry::Value;
        for (provider, effort, expected) in [
            ("openai", None, None),
            ("openai", Some("low"), Some("low")),
            ("openai", Some("medium"), Some("medium")),
            ("openai", Some("high"), Some("high")),
            ("openai", Some("max"), Some("xhigh")),
            ("openai", Some("SECRET_ARGUMENT"), None),
            ("google", Some("low"), Some("low")),
            ("google", Some("medium"), Some("high")),
            ("google", Some("high"), Some("high")),
            ("google", Some("max"), Some("high")),
        ] {
            let (_, spans) = capture(async {
                let span = tracing::info_span!("settings");
                let config = serde_json::json!({"model":"gemini-3-pro", "reasoning_effort":effort});
                super::genai::GenerationSettings::from_config(provider, &config).record(&span);
            })
            .await;
            assert_eq!(
                attr(&spans[0], "gen_ai.request.reasoning.level"),
                expected.map(Value::from).as_ref()
            );
            assert_private(&spans);
        }
        for (provider, model, effort, expected) in [
            ("openai", "test", Some("high"), Some("high")),
            ("codex", "test", Some("high"), Some("high")),
            ("anthropic", "test", Some("high"), None),
            ("anthropic", "test", None, None),
            ("xai", "grok-3-mini", Some("high"), Some("high")),
            (
                "xai",
                " org/GROK-4.20-MULTI-AGENT-beta ",
                Some("high"),
                Some("high"),
            ),
            ("xai", "grok-4.3-latest", Some("high"), Some("high")),
            ("xai", "grok-4.6", Some("high"), None),
            ("xai", "SECRET_ARGUMENT", Some("high"), None),
            ("google", "gemini-2.5-pro", Some("high"), None),
            ("unknown", "test", Some("high"), None),
        ] {
            let config = serde_json::json!({"model":model, "thinking_budget":null, "max_tokens":u32::MAX, "temperature":0.5, "top_p":0.75, "reasoning_effort":effort,
                "extra_body":{"store":false, "promptCacheKey":"SECRET_ARGUMENT", "verbosity":"SECRET_RESPONSE"}});
            let (_, spans) = capture(async {
                let span = tracing::info_span!("settings");
                super::genai::GenerationSettings::from_config(provider, &config).record(&span);
            })
            .await;
            let span = &spans[0];
            assert_eq!(
                attr(span, "gen_ai.request.max_tokens"),
                matches!(provider, "openai" | "anthropic")
                    .then_some(&Value::I64(i64::from(u32::MAX)))
            );
            assert_eq!(
                attr(span, "gen_ai.request.temperature"),
                match (provider, effort) {
                    ("anthropic", Some(_)) => Some(&Value::F64(1.0)),
                    ("anthropic", None) | ("openai" | "codex", _) => Some(&Value::F64(0.5)),
                    _ => None,
                }
            );
            assert_eq!(
                attr(span, "gen_ai.request.top_p"),
                matches!(provider, "openai" | "codex" | "anthropic").then_some(&Value::F64(0.75))
            );
            assert_eq!(
                attr(span, "gen_ai.request.reasoning.level"),
                expected.map(Value::from).as_ref()
            );
            assert_private(&spans);
        }
        for config in [
            serde_json::json!({}),
            serde_json::json!({"max_tokens":-1, "temperature":"1", "top_p":2, "reasoning_effort":"SECRET_ARGUMENT"}),
            serde_json::json!({"max_tokens":u64::MAX, "temperature":-0.5, "top_p":-1}),
            serde_json::json!({"max_tokens":12, "temperature":0.5, "top_p":0.75, "reasoning_effort":"high", "extra_body":{"reasoning":{"effort":"SECRET_ARGUMENT"}}}),
        ] {
            let (_, spans) = capture(async {
                let span = tracing::info_span!("settings");
                super::genai::GenerationSettings::from_config("openai", &config).record(&span);
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
    async fn genai_effort_omits_unverified_config_and_google_budgets() {
        use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
        use serde_json::json;
        for (provider, model) in [("xai", "grok-4.3"), ("google", "gemini-3-pro")] {
            let mut configs = vec![
                json!({"reasoning_effort":"high"}),
                json!({"model":42,"reasoning_effort":"high"}),
                json!({"model":"","reasoning_effort":"high"}),
                json!({"model":model}),
                json!({"model":model,"reasoning_effort":42}),
                json!({"model":model,"reasoning_effort":""}),
                json!({"model":model,"reasoning_effort":"high","extra_body":{"reasoning":{"effort":"SECRET_ARGUMENT"}}}),
            ];
            if provider == "google" {
                for budget in [json!(0), json!(123), json!("SECRET_ARGUMENT")] {
                    configs.push(
                        json!({"model":model,"reasoning_effort":"high","thinking_budget":budget}),
                    );
                }
            }
            for config in configs {
                let (_, spans) = capture(async {
                    let span = tracing::info_span!("settings");
                    super::genai::GenerationSettings::from_config(provider, &config).record(&span);
                })
                .await;
                assert!(attr(&spans[0], "gen_ai.request.reasoning.level").is_none());
                assert_private(&spans);
            }
        }
    }

    #[tokio::test]
    async fn genai_reasoning_usage_alias_and_cache_inclusive_totals_are_typed_and_widened() {
        use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
        use opentelemetry::Value;
        use querymt::chat::{ChatOutput, FinishReason};
        for reasoning in [None, Some(0), Some(u32::MAX)] {
            let mut output = ChatOutput::from_projections(
                Some("SECRET_ARGUMENT".into()),
                Some("SECRET_RESPONSE".into()),
                None,
                None,
                Some(FinishReason::Stop),
            );
            output.usage = reasoning.map(|reasoning_tokens| querymt::Usage {
                input_tokens: u32::MAX,
                output_tokens: u32::MAX,
                reasoning_tokens,
                cache_read: 20,
                cache_write: 30,
                cache_write_tiers: None,
            });
            let (_, spans) = capture(async {
                let chat = tracing::info_span!("chat");
                chat.in_scope(|| super::genai::output(&output));
            })
            .await;
            let chat = &spans[0];
            let expected = reasoning.map(|value| Value::I64(i64::from(value)));
            assert_eq!(
                attr(chat, "gen_ai.usage.reasoning.output_tokens"),
                expected.as_ref()
            );
            assert_eq!(
                attr(chat, "gen_ai.usage.reasoning_tokens"),
                expected.as_ref()
            );
            assert_eq!(
                attr(chat, "gen_ai.usage.output_tokens"),
                reasoning
                    .map(|value| Value::I64(i64::from(u32::MAX) + i64::from(value)))
                    .as_ref()
            );
            assert_eq!(
                attr(chat, "gen_ai.usage.input_tokens"),
                reasoning
                    .map(|_| Value::I64(i64::from(u32::MAX) + 50))
                    .as_ref()
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
        for span in &spans {
            assert!(attr(span, "gen_ai.usage.reasoning_tokens").is_none());
            assert!(attr(span, "gen_ai.request.reasoning.level").is_none());
        }
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
