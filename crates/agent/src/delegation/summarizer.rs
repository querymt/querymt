//! Delegation summary generation
//!
//! This module provides functionality to generate an "Implementation Brief" from
//! a parent planning conversation before delegating to a coder agent. The brief
//! provides the coder with context about decisions made, files to modify, patterns
//! to follow, and implementation steps.

use crate::agent::utils::{genai, render_prompt_for_display, render_prompt_for_llm};
use crate::config::DelegationSummaryConfig;
use crate::model::{AgentMessage, MessagePart};
use crate::session::error::{SessionError, SessionResult};
use crate::session::provider::{ProviderRequest, SessionProvider};
use crate::session::pruning::{SimpleTokenEstimator, TokenEstimator};
use futures_util::{FutureExt, StreamExt};
use querymt::LLMProvider;
use querymt::chat::{
    ChatMessage, ChatOutput, ChatOutputStatus, ChatRole, ChatStreamAccumulator, FinishReason,
    StreamChunk,
};
use std::sync::Arc;
use std::time::Duration;
use tracing::{Instrument, instrument};

/// System prompt for the summarizer LLM
const SUMMARIZER_SYSTEM_PROMPT: &str = r#"You are a technical brief writer for a software development team. Your job is to 
read a planning conversation between a user and a planning agent, then produce a 
concise, structured Implementation Brief for a coding agent who will do the actual 
implementation.

Rules:
- Be specific: include file paths, function names, line numbers when available
- Be concise: the coding agent has limited context window
- Prioritize: what matters most for implementation comes first
- Include decisions: capture WHY choices were made, not just WHAT
- Include patterns: reference existing code the coder should follow
- Skip meta-discussion: omit back-and-forth about planning process itself"#;

/// Summarizes a parent planning session for delegation handoff
pub struct DelegationSummarizer {
    provider: Arc<dyn LLMProvider>,
    provider_name: String,
    model: String,
    timeout: Duration,
    min_history_tokens: usize,
    estimator: Arc<dyn TokenEstimator>,
}

impl DelegationSummarizer {
    /// Build a summarizer from configuration
    pub async fn from_config(
        config: &DelegationSummaryConfig,
        session_provider: &SessionProvider,
    ) -> SessionResult<Self> {
        // Build params JSON including system prompt and max_tokens
        let mut params = serde_json::json!({
            "system": vec![SUMMARIZER_SYSTEM_PROMPT],
        });

        if let Some(max_tokens) = config.max_tokens {
            params["max_tokens"] = max_tokens.into();
        }

        let provider = session_provider
            .build_provider(
                ProviderRequest::new(&config.provider, &config.model)
                    .with_params(Some(&params))
                    .with_api_key_override(config.api_key.as_deref()),
            )
            .await?;

        Ok(Self {
            provider,
            provider_name: config.provider.clone(),
            model: config.model.clone(),
            timeout: Duration::from_secs(config.timeout_secs),
            min_history_tokens: config.min_history_tokens,
            estimator: Arc::new(SimpleTokenEstimator),
        })
    }

    /// Generate a structured Implementation Brief from parent session history
    #[instrument(
        name = "delegation.summarizer.summarize",
        skip(self, parent_history, delegation_objective),
        fields(
            history_messages = parent_history.len(),
            estimated_tokens = tracing::field::Empty,
            strategy = tracing::field::Empty,
            llm_transport = tracing::field::Empty,
            llm_duration_ms = tracing::field::Empty,
            output_bytes = tracing::field::Empty,
        )
    )]
    pub async fn summarize(
        &self,
        parent_history: &[AgentMessage],
        delegation_objective: &str,
    ) -> SessionResult<String> {
        let span = tracing::Span::current();

        // If the last message in history is a compaction (no messages after it),
        // its summary is already adequate — skip the LLM call.
        if let Some(summary) = Self::compaction_as_summary(parent_history) {
            span.record("strategy", "compaction");
            span.record("output_bytes", summary.len() as u64);
            log::info!("Using existing compaction summary for delegation (skipping LLM call)");
            return Ok(summary);
        }

        // Check token threshold — below threshold, inject raw formatted history
        // directly into the delegate context (no LLM summarization needed)
        let estimated_tokens = self.estimate_history_tokens(parent_history);
        span.record("estimated_tokens", estimated_tokens as u64);
        if estimated_tokens < self.min_history_tokens {
            span.record("strategy", "raw");
            log::debug!(
                "Parent history below summarization threshold ({} tokens < {}), injecting raw history",
                estimated_tokens,
                self.min_history_tokens
            );
            let result = self.format_conversation(parent_history, delegation_objective);
            span.record("output_bytes", result.len() as u64);
            return Ok(result);
        }

        span.record("strategy", "llm");

        // 1. Prepare LLM prompt from parent history
        let input = self.prepare_llm_input(parent_history, delegation_objective);

        // 2. Call LLM with timeout
        let messages = vec![ChatMessage::user().text(input).build()];

        let timeout = self.timeout;
        let use_streaming = self.provider.supports_streaming();
        span.record(
            "llm_transport",
            if use_streaming {
                "streaming"
            } else {
                "non_streaming"
            },
        );

        let llm_start = std::time::Instant::now();
        let session_id = parent_history
            .first()
            .map(|message| message.session_id.as_str())
            .filter(|id| !id.is_empty());
        let inference = tracing::info_span!(
            "delegation.summarizer.chat",
            otel.name = %format!("chat {}", self.model),
            otel.kind = "client",
            gen_ai.operation.name = "chat",
            gen_ai.provider.name = %genai::provider_name(&self.provider_name),
            gen_ai.request.model = %self.model,
            gen_ai.request.stream = use_streaming,
            gen_ai.conversation.id = session_id,
            session.id = session_id,
        );
        let output = async {
            // Default finish survives cancellation/drop without declaring an operation error.
            genai::finish_reason("error");
            match tokio::time::timeout(
                timeout,
                Self::call_provider(&self.provider, &messages, use_streaming),
            )
            .await
            {
                Ok(Ok(output)) => Ok(output),
                Ok(Err(error)) => {
                    genai::llm_error(&error);
                    Err(SessionError::InvalidOperation(format!(
                        "Delegation summary LLM call failed: {error}"
                    )))
                }
                Err(_) => {
                    genai::error("timeout");
                    Err(SessionError::InvalidOperation(format!(
                        "Delegation summary generation timed out after {} seconds",
                        timeout.as_secs()
                    )))
                }
            }
        }
        .instrument(inference)
        .await?;
        span.record("llm_duration_ms", llm_start.elapsed().as_millis() as u64);

        let summary = output
            .text()
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| "No summary generated".to_string());
        span.record("output_bytes", summary.len() as u64);
        Ok(summary)
    }

    async fn call_provider(
        provider: &Arc<dyn LLMProvider>,
        messages: &[ChatMessage],
        use_streaming: bool,
    ) -> Result<ChatOutput, querymt::error::LLMError> {
        let (output, stream_completed) = if use_streaming {
            let chat_span = tracing::Span::current();
            let mut first_chunk = genai::FirstChunk::default();
            first_chunk.start();
            let mut stream = provider.chat_stream(messages).await?;
            let mut accumulator = ChatStreamAccumulator::new();
            let mut terminal_seen = false;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.inspect_err(|_| genai::output(&accumulator.output()))?;
                first_chunk.received(&chat_span);
                accumulator.push(&chunk).map_err(|_| {
                    genai::output(&accumulator.output());
                    querymt::error::LLMError::GenericError("Invalid summary stream".into())
                })?;
                if querymt::chat::chunk_is_terminal(&chunk) {
                    terminal_seen = true;
                    break;
                }
            }
            if !terminal_seen {
                genai::output(&accumulator.output());
                return Err(querymt::error::LLMError::GenericError(
                    "Invalid summary stream: missing terminal event".into(),
                ));
            }
            // Keep failed/incomplete terminal metadata, but retain canonical item validation.
            let finish = accumulator.finish();
            let completed = finish.is_completed();
            let mut output = finish.into_output();
            // Merge only ready usage; never wait for a trailing chunk, even on rejection.
            while let Some(Some(Ok(StreamChunk::Usage(extra)))) = stream.next().now_or_never() {
                output.usage = Some(match output.usage.take() {
                    Some(previous) => previous.merge_max(extra),
                    None => extra,
                });
            }
            (output, completed)
        } else {
            (provider.chat(messages).await?, true)
        };

        // Record billed usage and actual finish before applying the summary-only success policy.
        genai::output(&output);
        if !stream_completed
            || output
                .status
                .is_some_and(|status| status != ChatOutputStatus::Completed)
            || matches!(
                output.finish_reason,
                Some(
                    FinishReason::Length
                        | FinishReason::ContentFilter
                        | FinishReason::Error
                        | FinishReason::ToolCalls
                )
            )
            || output.tool_calls().is_some_and(|calls| !calls.is_empty())
        {
            return Err(querymt::error::LLMError::GenericError(format!(
                "Delegation summary response did not complete successfully (status={:?}, finish_reason={:?})",
                output.status, output.finish_reason
            )));
        }
        Ok(output)
    }

    /// Estimate token count for a list of messages using the configured estimator
    fn estimate_history_tokens(&self, history: &[AgentMessage]) -> usize {
        history
            .iter()
            .map(|m| {
                m.parts
                    .iter()
                    .map(|p| match p {
                        MessagePart::Text { content } => self.estimator.estimate(content),
                        MessagePart::Prompt { blocks } => self
                            .estimator
                            .estimate(&render_prompt_for_llm(blocks, None)),
                        MessagePart::ToolResult { content, .. } => {
                            let text: String = content
                                .iter()
                                .filter_map(|b| b.as_text())
                                .collect::<Vec<_>>()
                                .join("\n");
                            self.estimator.estimate(&text)
                        }
                        MessagePart::Reasoning { item, .. } => {
                            self.estimator.estimate(&item.visible_text())
                        }
                        MessagePart::Compaction { summary, .. } => self.estimator.estimate(summary),
                        MessagePart::Output { output } => {
                            self.estimator.estimate(&output.estimate_text())
                        }
                        _ => 0,
                    })
                    .sum::<usize>()
            })
            .sum()
    }

    /// If the last message in history is a compaction (no messages after it),
    /// return its summary directly — it's already adequate context.
    fn compaction_as_summary(history: &[AgentMessage]) -> Option<String> {
        // Find the index of the last message that contains a Compaction part
        let last_compaction_idx = history.iter().rposition(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, MessagePart::Compaction { .. }))
        })?;

        // If there are messages after the compaction, we can't skip
        if last_compaction_idx < history.len() - 1 {
            return None;
        }

        // Extract the compaction summary text
        history[last_compaction_idx].parts.iter().find_map(|p| {
            if let MessagePart::Compaction { summary, .. } = p {
                Some(summary.clone())
            } else {
                None
            }
        })
    }

    /// Format the planning conversation as a readable transcript.
    ///
    /// This produces a clean context dump suitable for direct injection into
    /// a delegate agent's context. It does NOT include LLM meta-instructions.
    /// History is expected to be pre-filtered via `get_effective_history`.
    fn format_conversation(&self, history: &[AgentMessage], objective: &str) -> String {
        let mut conversation = String::new();

        for msg in history {
            match msg.role {
                ChatRole::User => {
                    // Include full user messages — they contain decisions and requirements
                    conversation
                        .push_str(&format!("\n[User]: {}\n", Self::extract_text_content(msg)));
                }
                ChatRole::Assistant => {
                    for part in &msg.parts {
                        match part {
                            MessagePart::Text { content } => {
                                conversation.push_str(&format!("\n[Planner]: {}\n", content));
                            }
                            MessagePart::Prompt { blocks } => {
                                let display_content = render_prompt_for_display(blocks);
                                if !display_content.trim().is_empty() {
                                    conversation
                                        .push_str(&format!("\n[Planner]: {}\n", display_content));
                                }
                            }
                            MessagePart::ToolUse(tu) => {
                                // Just the tool name + key args, not full output
                                let args_summary = if let Ok(args_value) =
                                    serde_json::from_str::<serde_json::Value>(
                                        &tu.function.arguments,
                                    ) {
                                    Self::summarize_tool_args(&args_value)
                                } else {
                                    tu.function.arguments.clone()
                                };
                                conversation.push_str(&format!(
                                    "\n[Tool Call]: {} ({})\n",
                                    tu.function.name, args_summary
                                ));
                            }
                            MessagePart::Output { output } => {
                                // Structured turn: project calls and visible text
                                // in canonical item order.
                                if let Some(text) = output.text() {
                                    conversation.push_str(&format!("\n[Planner]: {}\n", text));
                                }
                                for call in output.tool_calls().unwrap_or_default() {
                                    let args_summary = if let Ok(args_value) =
                                        serde_json::from_str::<serde_json::Value>(
                                            &call.function.arguments,
                                        ) {
                                        Self::summarize_tool_args(&args_value)
                                    } else {
                                        call.function.arguments.clone()
                                    };
                                    conversation.push_str(&format!(
                                        "\n[Tool Call]: {} ({})\n",
                                        call.function.name, args_summary
                                    ));
                                }
                            }
                            MessagePart::Compaction {
                                summary,
                                original_token_count: _,
                            } => {
                                // Include compaction summaries — they're already condensed
                                conversation.push_str(&format!(
                                    "\n[Previous Context Summary]: {}\n",
                                    summary
                                ));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        format!("Delegation objective: {objective}\n\nPlanning conversation:\n{conversation}")
    }

    /// Prepare the full prompt for the summarizer LLM.
    ///
    /// Wraps the formatted conversation with instructions for the summarizer
    /// to produce a structured Implementation Brief.
    fn prepare_llm_input(&self, history: &[AgentMessage], objective: &str) -> String {
        let conversation = self.format_conversation(history, objective);
        format!(
            r#"You are a technical brief writer. A planning agent had the following \
conversation while researching a task. The task will now be delegated \
to a coding agent for implementation.

{conversation}

Write a structured Implementation Brief for the coding agent. Include:
1. **Objective** — one clear sentence
2. **Key Decisions** — what was decided during planning
3. **Files to Modify** — specific file paths and what to change in each
4. **Patterns to Follow** — code patterns, conventions, or reference implementations found
5. **Constraints** — technical constraints, user preferences, things to avoid
6. **Implementation Steps** — ordered list of concrete steps

Be specific. Include file paths, function names, and code patterns \
the planner discovered. The coding agent has no access to this \
planning conversation."#
        )
    }

    /// Extract text content from all message parts
    fn extract_text_content(msg: &AgentMessage) -> String {
        let mut rendered_parts = Vec::new();
        for part in &msg.parts {
            match part {
                MessagePart::Text { content } => rendered_parts.push(content.clone()),
                MessagePart::Prompt { blocks } => {
                    rendered_parts.push(render_prompt_for_display(blocks));
                }
                _ => {}
            }
        }
        rendered_parts.join("\n")
    }

    /// Summarize tool arguments to just the key info
    fn summarize_tool_args(input: &serde_json::Value) -> String {
        // Extract common useful fields
        if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
            return path.to_string();
        }
        if let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) {
            return pattern.to_string();
        }
        if let Some(file_path) = input.get("filePath").and_then(|v| v.as_str()) {
            return file_path.to_string();
        }
        if let Some(command) = input.get("command").and_then(|v| v.as_str()) {
            return if command.len() > 100 {
                let end = command.floor_char_boundary(100);
                format!("{}...", &command[..end])
            } else {
                command.to_string()
            };
        }

        // Fallback: truncated JSON
        let s = serde_json::to_string(input).unwrap_or_default();
        if s.len() > 200 {
            let end = s.floor_char_boundary(200);
            format!("{}...", &s[..end])
        } else {
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use querymt::chat::{ChatOutput, FinishReason, Tool};
    use querymt::completion::{CompletionRequest, CompletionResponse};
    use querymt::error::LLMError;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    type SummaryStream =
        Pin<Box<dyn futures_util::Stream<Item = Result<StreamChunk, LLMError>> + Send>>;
    type SummaryChunks = Result<Vec<Result<StreamChunk, LLMError>>, LLMError>;

    struct SummaryTestProvider {
        supports_streaming: bool,
        chat_result: Mutex<Option<Result<ChatOutput, LLMError>>>,
        stream_result: Mutex<Option<SummaryChunks>>,
        stall_stream: bool,
        pending_tail: bool,
        setup_delay: Duration,
        first_delay: Duration,
        chat_calls: AtomicUsize,
        stream_calls: AtomicUsize,
    }

    impl SummaryTestProvider {
        fn non_streaming_text(text: &str) -> Self {
            Self::non_streaming(Ok(text.to_string()))
        }

        fn non_streaming_error(error: LLMError) -> Self {
            Self::non_streaming(Err(error))
        }

        fn non_streaming(result: Result<String, LLMError>) -> Self {
            Self::output(
                result.map(|text| {
                    crate::test_utils::mocks::MockChatResponse::text_only(&text).into()
                }),
            )
        }

        fn output(result: Result<ChatOutput, LLMError>) -> Self {
            Self {
                supports_streaming: false,
                chat_result: Mutex::new(Some(result)),
                stream_result: Mutex::new(None),
                stall_stream: false,
                pending_tail: false,
                setup_delay: Duration::ZERO,
                first_delay: Duration::ZERO,
                chat_calls: AtomicUsize::new(0),
                stream_calls: AtomicUsize::new(0),
            }
        }

        fn streaming(chunks: Vec<Result<StreamChunk, LLMError>>) -> Self {
            Self {
                supports_streaming: true,
                chat_result: Mutex::new(None),
                stream_result: Mutex::new(Some(Ok(chunks))),
                stall_stream: false,
                pending_tail: false,
                setup_delay: Duration::ZERO,
                first_delay: Duration::ZERO,
                chat_calls: AtomicUsize::new(0),
                stream_calls: AtomicUsize::new(0),
            }
        }

        fn stalled_stream() -> Self {
            Self {
                supports_streaming: true,
                chat_result: Mutex::new(None),
                stream_result: Mutex::new(None),
                stall_stream: true,
                pending_tail: false,
                setup_delay: Duration::ZERO,
                first_delay: Duration::ZERO,
                chat_calls: AtomicUsize::new(0),
                stream_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl querymt::chat::ChatProvider for SummaryTestProvider {
        fn supports_streaming(&self) -> bool {
            self.supports_streaming
        }

        async fn chat_with_tools(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[Tool]>,
        ) -> Result<ChatOutput, LLMError> {
            self.chat_calls.fetch_add(1, Ordering::SeqCst);
            self.chat_result
                .lock()
                .expect("chat result lock")
                .take()
                .unwrap_or_else(|| {
                    Err(LLMError::NotImplemented(
                        "non-streaming chat was not configured".to_string(),
                    ))
                })
        }

        async fn chat_stream_with_tools(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[Tool]>,
        ) -> Result<SummaryStream, LLMError> {
            self.stream_calls.fetch_add(1, Ordering::SeqCst);
            if !self.setup_delay.is_zero() {
                tokio::time::sleep(self.setup_delay).await;
            }
            if self.stall_stream {
                return Ok(Box::pin(futures_util::stream::pending()));
            }

            let chunks = self
                .stream_result
                .lock()
                .expect("stream result lock")
                .take()
                .unwrap_or_else(|| {
                    Err(LLMError::NotImplemented(
                        "streaming chat was not configured".to_string(),
                    ))
                })?;
            let mut first_delay = self.first_delay;
            let stream = futures_util::stream::iter(chunks).then(move |chunk| {
                let delay = std::mem::take(&mut first_delay);
                async move {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    chunk
                }
            });
            if self.pending_tail {
                Ok(Box::pin(stream.chain(futures_util::stream::pending())))
            } else {
                Ok(Box::pin(stream))
            }
        }
    }

    #[async_trait]
    impl querymt::completion::CompletionProvider for SummaryTestProvider {
        async fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
            Err(LLMError::NotImplemented("completion not supported".into()))
        }
    }

    #[async_trait]
    impl querymt::embedding::EmbeddingProvider for SummaryTestProvider {
        async fn embed(&self, _input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
            Err(LLMError::NotImplemented("embedding not supported".into()))
        }
    }

    impl LLMProvider for SummaryTestProvider {}

    #[tokio::test]
    async fn genai_summary_non_streaming_records_typed_metadata_without_content() {
        use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
        use opentelemetry::{
            Array, Value,
            trace::{SpanKind, Status},
        };
        let mut output: ChatOutput =
            crate::test_utils::mocks::MockChatResponse::text_only("SECRET_RESPONSE").into();
        output.finish_reason = Some(FinishReason::Stop);
        output.response_id = Some("summary-response-id".into());
        output.provenance = Some(querymt::chat::ChatOutputProvenance {
            provider: "codex".into(),
            model: "reported-summary-model".into(),
            protocol: "SECRET_ARGUMENT".into(),
            endpoint: "SECRET_ENDPOINT".into(),
        });
        output.usage = Some(querymt::Usage {
            input_tokens: u32::MAX,
            output_tokens: 20,
            cache_read: 7,
            cache_write: 3,
            reasoning_tokens: 5,
        });
        let summarizer = summarizer_with_provider(
            Arc::new(SummaryTestProvider::output(Ok(output))),
            Duration::from_secs(1),
        );
        let (result, spans) =
            capture(summarizer.summarize(&[make_user_msg("SECRET_PROMPT")], "SECRET_ARGUMENT"))
                .await;
        assert_eq!(result.unwrap(), "SECRET_RESPONSE");
        let chat = spans
            .iter()
            .find(|span| span.name == "chat requested-summary-model")
            .unwrap();
        assert_eq!(chat.span_kind, SpanKind::Client);
        assert_eq!(chat.status, Status::Unset);
        assert_eq!(
            attr(chat, "gen_ai.provider.name"),
            Some(&Value::from("openai"))
        );
        assert_eq!(
            attr(chat, "gen_ai.request.model"),
            Some(&Value::from("requested-summary-model"))
        );
        assert_eq!(
            attr(chat, "gen_ai.response.model"),
            Some(&Value::from("reported-summary-model"))
        );
        assert_eq!(
            attr(chat, "gen_ai.response.id"),
            Some(&Value::from("summary-response-id"))
        );
        assert_eq!(
            attr(chat, "gen_ai.conversation.id"),
            Some(&Value::from("s1"))
        );
        assert_eq!(
            attr(chat, "session.id"),
            attr(chat, "gen_ai.conversation.id")
        );
        assert_eq!(
            attr(chat, "gen_ai.request.stream"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            attr(chat, "gen_ai.usage.input_tokens"),
            Some(&Value::I64(i64::from(u32::MAX) + 10))
        );
        assert_eq!(
            attr(chat, "gen_ai.usage.output_tokens"),
            Some(&Value::I64(25))
        );
        assert_eq!(
            attr(chat, "gen_ai.usage.cache_read.input_tokens"),
            Some(&Value::I64(7))
        );
        assert_eq!(
            attr(chat, "gen_ai.usage.cache_write.input_tokens"),
            Some(&Value::I64(3))
        );
        assert_eq!(
            attr(chat, "gen_ai.usage.reasoning.output_tokens"),
            Some(&Value::I64(5))
        );
        assert_eq!(
            attr(chat, "gen_ai.response.finish_reasons"),
            Some(&Value::Array(Array::String(vec!["stop".into()])))
        );
        assert!(attr(chat, "gen_ai.response.time_to_first_chunk").is_none());
        assert_private(&spans);

        for session_id in [None, Some("")] {
            let summarizer = summarizer_with_provider(
                Arc::new(SummaryTestProvider::non_streaming_text("SECRET_RESPONSE")),
                Duration::from_secs(1),
            );
            let history: Vec<_> = session_id
                .into_iter()
                .map(|id| {
                    let mut message = make_user_msg("SECRET_PROMPT");
                    message.session_id = id.into();
                    message
                })
                .collect();
            let (result, spans) = capture(async {
                summarizer
                    .summarize(&history, "SECRET_ARGUMENT")
                    .instrument(tracing::info_span!(
                        "unrelated-parent",
                        session.id = "unrelated-session"
                    ))
                    .await
            })
            .await;
            assert_eq!(result.unwrap(), "SECRET_RESPONSE");
            let chat = spans
                .iter()
                .find(|span| span.name == "chat requested-summary-model")
                .unwrap();
            assert!(attr(chat, "gen_ai.conversation.id").is_none());
            assert!(attr(chat, "session.id").is_none());
            assert_private(&spans);
        }
    }

    fn partial_summary_output(
        status: Option<ChatOutputStatus>,
        finish: FinishReason,
    ) -> ChatOutput {
        let mut output: ChatOutput =
            crate::test_utils::mocks::MockChatResponse::text_only("SECRET_RESPONSE").into();
        output.status = status;
        output.finish_reason = Some(finish);
        output.response_id = Some("partial-summary-id".into());
        output.provenance = Some(querymt::chat::ChatOutputProvenance {
            provider: "codex".into(),
            model: "reported-summary-model".into(),
            protocol: "SECRET_ARGUMENT".into(),
            endpoint: "SECRET_ENDPOINT".into(),
        });
        output
    }

    async fn assert_summary_rejected(
        provider: SummaryTestProvider,
        expected: &ChatOutput,
        finish: &str,
    ) {
        use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
        use opentelemetry::{Array, Value, trace::Status};
        let streaming = provider.supports_streaming;
        let summarizer = summarizer_with_provider(Arc::new(provider), Duration::from_secs(1));
        let (result, spans) =
            capture(summarizer.summarize(&[make_user_msg("SECRET_PROMPT")], "SECRET_ARGUMENT"))
                .await;
        let error = result
            .expect_err("partial brief must not be returned")
            .to_string();
        assert!(error.contains("did not complete successfully"));
        assert!(!error.contains("SECRET_"));
        let chats: Vec<_> = spans
            .iter()
            .filter(|span| span.name == "chat requested-summary-model")
            .collect();
        assert_eq!(chats.len(), 1);
        let chat = chats[0];
        assert_eq!(chat.status, Status::error(""));
        assert_eq!(
            attr(chat, "error.type"),
            Some(&Value::from("provider_error"))
        );
        assert_eq!(
            attr(chat, "gen_ai.request.stream"),
            Some(&Value::Bool(streaming))
        );
        assert_eq!(
            attr(chat, "gen_ai.response.id"),
            expected.response_id.clone().map(Value::from).as_ref()
        );
        assert_eq!(
            attr(chat, "gen_ai.response.model"),
            expected
                .provenance
                .as_ref()
                .map(|origin| Value::from(origin.model.clone()))
                .as_ref()
        );
        let usage = expected.usage.as_ref().unwrap();
        assert_eq!(
            attr(chat, "gen_ai.usage.input_tokens"),
            Some(&Value::I64(i64::from(usage.input_tokens)))
        );
        assert_eq!(
            attr(chat, "gen_ai.usage.output_tokens"),
            Some(&Value::I64(i64::from(usage.output_tokens)))
        );
        assert_eq!(
            attr(chat, "gen_ai.response.finish_reasons"),
            Some(&Value::Array(Array::String(vec![finish.to_owned().into()])))
        );
        assert_private(&spans);
    }

    #[tokio::test]
    async fn genai_summary_rejects_unsuccessful_non_streaming_outputs_without_losing_metadata() {
        for (status, finish, reason, tool_call) in [
            (
                Some(ChatOutputStatus::Incomplete),
                FinishReason::Stop,
                "stop",
                false,
            ),
            (
                Some(ChatOutputStatus::Failed),
                FinishReason::Stop,
                "stop",
                false,
            ),
            (
                Some(ChatOutputStatus::InProgress),
                FinishReason::Stop,
                "stop",
                false,
            ),
            (
                Some(ChatOutputStatus::Completed),
                FinishReason::Length,
                "length",
                false,
            ),
            (
                Some(ChatOutputStatus::Completed),
                FinishReason::ContentFilter,
                "content_filter",
                false,
            ),
            (
                Some(ChatOutputStatus::Completed),
                FinishReason::ToolCalls,
                "tool_calls",
                false,
            ),
            (
                Some(ChatOutputStatus::Completed),
                FinishReason::Error,
                "error",
                false,
            ),
            (
                Some(ChatOutputStatus::Completed),
                FinishReason::Stop,
                "stop",
                true,
            ),
        ] {
            let mut output = partial_summary_output(status, finish);
            if tool_call {
                output
                    .items
                    .push(querymt::chat::ChatOutputItem::FunctionCall(
                        querymt::chat::ChatFunctionCallItem {
                            item_id: None,
                            call_id: "summary-call".into(),
                            name: "SECRET_ARGUMENT".into(),
                            arguments: r#"{"value":"SECRET_ARGUMENT"}"#.into(),
                            status: Some(ChatOutputStatus::Completed),
                            extensions: Default::default(),
                        },
                    ));
            }
            assert_summary_rejected(
                SummaryTestProvider::output(Ok(output.clone())),
                &output,
                reason,
            )
            .await;
        }
    }

    #[tokio::test]
    async fn genai_summary_accepts_legacy_status_none_with_stop() {
        use crate::test_utils::helpers::genai_trace::{assert_private, capture};
        use opentelemetry::trace::Status;
        let output = partial_summary_output(None, FinishReason::Stop);
        let summarizer = summarizer_with_provider(
            Arc::new(SummaryTestProvider::output(Ok(output))),
            Duration::from_secs(1),
        );
        let (result, spans) =
            capture(summarizer.summarize(&[make_user_msg("SECRET_PROMPT")], "SECRET_ARGUMENT"))
                .await;
        assert_eq!(result.unwrap(), "SECRET_RESPONSE");
        assert_eq!(
            spans
                .iter()
                .find(|span| span.name == "chat requested-summary-model")
                .unwrap()
                .status,
            Status::Unset
        );
        assert_private(&spans);
    }

    #[tokio::test]
    async fn genai_summary_rejects_unsuccessful_streams_and_keeps_ready_billed_usage() {
        use querymt::chat::StructuredStreamEvent;
        for structured in [false, true] {
            for (status, finish, reason, unfinished_item) in [
                (
                    ChatOutputStatus::Completed,
                    FinishReason::Length,
                    "length",
                    false,
                ),
                (
                    ChatOutputStatus::Completed,
                    FinishReason::ContentFilter,
                    "content_filter",
                    false,
                ),
                (
                    ChatOutputStatus::Completed,
                    FinishReason::ToolCalls,
                    "tool_calls",
                    false,
                ),
                (
                    ChatOutputStatus::Completed,
                    FinishReason::Error,
                    "error",
                    false,
                ),
                (
                    ChatOutputStatus::Incomplete,
                    FinishReason::Stop,
                    "stop",
                    false,
                ),
                (ChatOutputStatus::Failed, FinishReason::Stop, "stop", false),
                (
                    ChatOutputStatus::InProgress,
                    FinishReason::Stop,
                    "stop",
                    false,
                ),
                (
                    ChatOutputStatus::Completed,
                    FinishReason::Stop,
                    "stop",
                    true,
                ),
            ] {
                if !structured && (status != ChatOutputStatus::Completed || unfinished_item) {
                    continue;
                }
                let mut output = partial_summary_output(Some(status), finish);
                let mut chunks = if structured {
                    vec![
                        StreamChunk::Structured(StructuredStreamEvent::ResponseMetadata {
                            response_id: output.response_id.clone(),
                            status: Some(ChatOutputStatus::InProgress),
                            usage: None,
                            finish_reason: None,
                            provenance: output.provenance.clone(),
                        }),
                        StreamChunk::Structured(if unfinished_item {
                            StructuredStreamEvent::ItemStarted {
                                output_index: 0,
                                item: output.items[0].clone(),
                            }
                        } else {
                            StructuredStreamEvent::ItemCompleted {
                                output_index: 0,
                                item: output.items[0].clone(),
                            }
                        }),
                        StreamChunk::Structured(StructuredStreamEvent::ResponseTerminal {
                            status,
                            usage: output.usage.clone(),
                            finish_reason: Some(finish),
                            detail: Some("SECRET_ERROR".into()),
                        }),
                    ]
                } else {
                    output.response_id = None;
                    output.provenance = None;
                    vec![
                        StreamChunk::Text("SECRET_RESPONSE".into()),
                        StreamChunk::Usage(output.usage.clone().unwrap()),
                        StreamChunk::Done {
                            finish_reason: finish,
                        },
                    ]
                };
                output.usage = Some(querymt::Usage {
                    input_tokens: 200,
                    output_tokens: 70,
                    ..Default::default()
                });
                chunks.push(StreamChunk::Usage(output.usage.clone().unwrap()));
                let mut provider =
                    SummaryTestProvider::streaming(chunks.into_iter().map(Ok).collect());
                provider.pending_tail = true;
                assert_summary_rejected(provider, &output, reason).await;
            }
        }
    }

    #[tokio::test]
    async fn genai_summary_streams_merge_ready_usage_and_ignore_projections_without_waiting() {
        use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
        use opentelemetry::{Value, trace::Status};
        use querymt::chat::{ChatOutputStatus, StructuredStreamEvent};
        for structured in [false, true] {
            let initial = querymt::Usage {
                input_tokens: 12,
                output_tokens: 20,
                ..Default::default()
            };
            let mut chunks = if structured {
                let output: ChatOutput =
                    crate::test_utils::mocks::MockChatResponse::text_only("SECRET_RESPONSE").into();
                vec![
                    StreamChunk::Structured(StructuredStreamEvent::ResponseMetadata {
                        response_id: Some("stream-summary-id".into()),
                        status: Some(ChatOutputStatus::InProgress),
                        usage: None,
                        finish_reason: None,
                        provenance: Some(querymt::chat::ChatOutputProvenance {
                            provider: "codex".into(),
                            model: "reported-stream-model".into(),
                            protocol: "SECRET_ARGUMENT".into(),
                            endpoint: "SECRET_ENDPOINT".into(),
                        }),
                    }),
                    StreamChunk::Text("SECRET_ARGUMENT".into()),
                    StreamChunk::Structured(StructuredStreamEvent::ItemCompleted {
                        output_index: 0,
                        item: output.items[0].clone(),
                    }),
                    StreamChunk::Structured(StructuredStreamEvent::ResponseTerminal {
                        status: ChatOutputStatus::Completed,
                        usage: Some(initial),
                        finish_reason: Some(FinishReason::Stop),
                        detail: None,
                    }),
                ]
            } else {
                vec![
                    StreamChunk::Thinking("SECRET_ARGUMENT".into()),
                    StreamChunk::Text("SECRET_RESPONSE".into()),
                    StreamChunk::Usage(initial),
                    StreamChunk::Done {
                        finish_reason: FinishReason::Stop,
                    },
                ]
            };
            chunks.extend([
                StreamChunk::Usage(querymt::Usage {
                    input_tokens: 30,
                    output_tokens: 5,
                    cache_read: 7,
                    ..Default::default()
                }),
                StreamChunk::Usage(querymt::Usage {
                    input_tokens: 10,
                    output_tokens: 40,
                    reasoning_tokens: 2,
                    ..Default::default()
                }),
            ]);
            let mut provider = SummaryTestProvider::streaming(chunks.into_iter().map(Ok).collect());
            provider.setup_delay = Duration::from_millis(20);
            provider.first_delay = Duration::from_millis(20);
            provider.pending_tail = true;
            let summarizer = summarizer_with_provider(Arc::new(provider), Duration::from_secs(1));
            let (result, spans) =
                capture(summarizer.summarize(&[make_user_msg("SECRET_PROMPT")], "SECRET_ARGUMENT"))
                    .await;
            assert_eq!(result.unwrap(), "SECRET_RESPONSE");
            let chat = spans
                .iter()
                .find(|span| span.name == "chat requested-summary-model")
                .unwrap();
            assert_eq!(chat.status, Status::Unset);
            assert!(
                matches!(attr(chat, "gen_ai.response.time_to_first_chunk"), Some(Value::F64(seconds)) if *seconds >= 0.04)
            );
            assert_eq!(
                chat.attributes
                    .iter()
                    .filter(|attr| attr.key.as_str() == "gen_ai.response.time_to_first_chunk")
                    .count(),
                1
            );
            assert!(attr(chat, "gen_ai.agent.id").is_none());
            assert_eq!(
                attr(chat, "gen_ai.request.stream"),
                Some(&Value::Bool(true))
            );
            assert_eq!(
                attr(chat, "gen_ai.usage.input_tokens"),
                Some(&Value::I64(37))
            );
            assert_eq!(
                attr(chat, "gen_ai.usage.output_tokens"),
                Some(&Value::I64(42))
            );
            if structured {
                assert_eq!(
                    attr(chat, "gen_ai.response.model"),
                    Some(&Value::from("reported-stream-model"))
                );
            }
            assert_private(&spans);
        }
    }

    #[tokio::test]
    async fn genai_summary_failure_timeout_cancel_and_drop_have_bounded_status() {
        use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
        use opentelemetry::{Array, Value, trace::Status};
        for (provider, expected_error, dropped) in [
            (
                SummaryTestProvider::non_streaming_error(LLMError::ProviderError(
                    "SECRET_ERROR".into(),
                )),
                Some("provider_error"),
                false,
            ),
            (
                SummaryTestProvider::streaming(vec![Err(LLMError::Cancelled)]),
                None,
                false,
            ),
            (
                SummaryTestProvider::stalled_stream(),
                Some("timeout"),
                false,
            ),
            (SummaryTestProvider::stalled_stream(), None, true),
        ] {
            let summarizer =
                summarizer_with_provider(Arc::new(provider), Duration::from_millis(20));
            let (_, spans) = capture(async {
                let history = [make_user_msg("SECRET_PROMPT")];
                let mut future = Box::pin(summarizer.summarize(&history, "SECRET_ARGUMENT"));
                if dropped {
                    tokio::select! {
                        _ = &mut future => panic!("must still be pending"),
                        _ = tokio::time::sleep(Duration::from_millis(1)) => {}
                    }
                } else {
                    assert!(future.await.is_err());
                }
            })
            .await;
            let chat = spans
                .iter()
                .find(|span| span.name == "chat requested-summary-model")
                .unwrap();
            assert_eq!(
                attr(chat, "gen_ai.response.finish_reasons"),
                Some(&Value::Array(Array::String(vec!["error".into()])))
            );
            assert_eq!(
                attr(chat, "error.type"),
                expected_error.map(Value::from).as_ref()
            );
            assert!(attr(chat, "gen_ai.response.time_to_first_chunk").is_none());
            assert_eq!(
                chat.status,
                if expected_error.is_some() {
                    Status::error("")
                } else {
                    Status::Unset
                }
            );
            assert_private(&spans);
        }
    }

    #[tokio::test]
    async fn genai_summary_shortcuts_do_not_create_inference_spans() {
        use crate::test_utils::helpers::genai_trace::{assert_private, capture};
        let provider = Arc::new(SummaryTestProvider::non_streaming_text("not called"));
        let mut summarizer = summarizer_with_provider(provider.clone(), Duration::from_secs(1));
        summarizer.min_history_tokens = usize::MAX;
        let history = [make_user_msg("SECRET_PROMPT")];
        let (_, raw_spans) = capture(summarizer.summarize(&history, "SECRET_ARGUMENT")).await;
        summarizer.min_history_tokens = 0;
        let (request, summary) =
            crate::session::compaction::SessionCompaction::create_compaction_messages(
                "s1",
                "SECRET_RESPONSE",
                100,
            );
        let (_, compact_spans) =
            capture(summarizer.summarize(&[request, summary], "SECRET_ARGUMENT")).await;
        for spans in [raw_spans, compact_spans] {
            assert!(spans.iter().all(|span| !span.name.starts_with("chat ")));
            assert_private(&spans);
        }
        assert_eq!(provider.chat_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 0);
    }

    // ── summarize_tool_args ────────────────────────────────────────────────

    #[test]
    fn summarize_tool_args_extracts_path() {
        let args = serde_json::json!({
            "path": "src/main.rs",
            "other": "ignored"
        });
        let summary = DelegationSummarizer::summarize_tool_args(&args);
        assert_eq!(summary, "src/main.rs");
    }

    #[test]
    fn summarize_tool_args_extracts_pattern() {
        let args = serde_json::json!({
            "pattern": "*.rs",
            "other": "ignored"
        });
        let summary = DelegationSummarizer::summarize_tool_args(&args);
        assert_eq!(summary, "*.rs");
    }

    #[test]
    fn summarize_tool_args_extracts_file_path() {
        let args = serde_json::json!({
            "filePath": "test.txt",
            "other": "ignored"
        });
        let summary = DelegationSummarizer::summarize_tool_args(&args);
        assert_eq!(summary, "test.txt");
    }

    #[test]
    fn summarize_tool_args_extracts_command() {
        let args = serde_json::json!({
            "command": "cargo build",
            "other": "ignored"
        });
        let summary = DelegationSummarizer::summarize_tool_args(&args);
        assert_eq!(summary, "cargo build");
    }

    #[test]
    fn summarize_tool_args_truncates_long_command() {
        let long_cmd = "a".repeat(150);
        let args = serde_json::json!({
            "command": long_cmd
        });
        let summary = DelegationSummarizer::summarize_tool_args(&args);
        // floor_char_boundary may round down, so length is <= 100 + "...".len()
        assert!(summary.len() <= 103);
        assert!(summary.ends_with("..."));
        // The retained prefix must itself be valid UTF-8 (no panic on indexing)
        assert!(std::str::from_utf8(summary.trim_end_matches("...").as_bytes()).is_ok());
    }

    #[test]
    fn summarize_tool_args_truncates_multibyte_json() {
        // em dash (—) is 3 bytes: 0xE2 0x80 0x94.
        // Previously `&s[..200]` would panic when the boundary landed inside it.
        // Build a JSON string that is > 200 bytes and contains an em dash near byte 200.
        let filler = "x".repeat(195); // 195 ASCII bytes in JSON value
        let args = serde_json::json!({
            "todos": [{"id": "1", "content": format!("{}—suffix", filler)}]
        });
        // This must not panic.
        let summary = DelegationSummarizer::summarize_tool_args(&args);
        assert!(summary.ends_with("..."));
        assert!(summary.len() <= 203);
        // Result must be valid UTF-8.
        assert!(std::str::from_utf8(summary.as_bytes()).is_ok());
    }

    #[test]
    fn summarize_tool_args_fallback_to_json() {
        let args = serde_json::json!({
            "unknown_field": "value",
            "another": 123
        });
        let summary = DelegationSummarizer::summarize_tool_args(&args);
        // Should be JSON representation
        assert!(summary.contains("unknown_field"));
        assert!(summary.contains("value"));
    }

    #[test]
    fn summarize_tool_args_truncates_long_json() {
        let mut obj = serde_json::Map::new();
        for i in 0..50 {
            obj.insert(format!("field_{}", i), serde_json::json!("long_value"));
        }
        let args = serde_json::Value::Object(obj);
        let summary = DelegationSummarizer::summarize_tool_args(&args);
        // floor_char_boundary may round down, so length is <= 200 + "...".len()
        assert!(summary.len() <= 203);
        assert!(summary.ends_with("..."));
        // The retained prefix must itself be valid UTF-8 (no panic on indexing)
        assert!(std::str::from_utf8(summary.trim_end_matches("...").as_bytes()).is_ok());
    }

    // ── format_conversation / prepare_llm_input ──────────────────────────────

    fn make_user_msg(text: &str) -> AgentMessage {
        AgentMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::Text {
                content: text.to_string(),
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        }
    }

    fn make_assistant_msg(text: &str) -> AgentMessage {
        AgentMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Text {
                content: text.to_string(),
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        }
    }

    fn summarizer_with_provider(
        provider: Arc<dyn LLMProvider>,
        timeout: Duration,
    ) -> DelegationSummarizer {
        DelegationSummarizer {
            provider,
            provider_name: "codex".into(),
            model: "requested-summary-model".into(),
            timeout,
            min_history_tokens: 0,
            estimator: Arc::new(SimpleTokenEstimator),
        }
    }

    /// Helper: build a DelegationSummarizer with dummy provider (only used for
    /// testing format_conversation / prepare_llm_input which don't call the LLM).
    fn test_summarizer() -> DelegationSummarizer {
        let provider: Arc<dyn LLMProvider> =
            Arc::new(crate::test_utils::mocks::MockLlmProvider::new());
        summarizer_with_provider(provider, Duration::from_secs(30))
    }

    #[tokio::test]
    async fn streaming_provider_collects_text_and_ignores_non_text_chunks() {
        let provider = Arc::new(SummaryTestProvider::streaming(vec![
            Ok(StreamChunk::Thinking("internal".to_string())),
            Ok(StreamChunk::Text("implementation ".to_string())),
            Ok(StreamChunk::Usage(querymt::Usage::default())),
            Ok(StreamChunk::Text("brief".to_string())),
            Ok(StreamChunk::Done {
                finish_reason: FinishReason::Stop,
            }),
            Ok(StreamChunk::Text("ignored after done".to_string())),
        ]));
        let summarizer = summarizer_with_provider(provider.clone(), Duration::from_secs(1));

        let summary = summarizer
            .summarize(&[make_user_msg("plan")], "implement")
            .await
            .expect("streaming summary");

        assert_eq!(summary, "implementation brief");
        assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.chat_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn streaming_provider_rejects_end_without_terminal_chunk() {
        let provider = Arc::new(SummaryTestProvider::streaming(vec![Ok(StreamChunk::Text(
            "complete at eof".to_string(),
        ))]));
        let summarizer = summarizer_with_provider(provider, Duration::from_secs(1));

        let error = summarizer
            .summarize(&[make_user_msg("plan")], "implement")
            .await
            .expect_err("unterminated stream");

        assert!(error.to_string().contains("Invalid summary stream"));
    }

    #[tokio::test]
    async fn non_streaming_provider_uses_chat_response() {
        let provider = Arc::new(SummaryTestProvider::non_streaming_text(
            "implementation brief",
        ));
        let summarizer = summarizer_with_provider(provider.clone(), Duration::from_secs(1));

        let summary = summarizer
            .summarize(&[make_user_msg("plan")], "implement")
            .await
            .expect("non-streaming summary");

        assert_eq!(summary, "implementation brief");
        assert_eq!(provider.chat_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn empty_response_uses_existing_fallback_for_both_transports() {
        let streaming = summarizer_with_provider(
            Arc::new(SummaryTestProvider::streaming(vec![Ok(
                StreamChunk::Done {
                    finish_reason: FinishReason::Stop,
                },
            )])),
            Duration::from_secs(1),
        );
        let non_streaming = summarizer_with_provider(
            Arc::new(SummaryTestProvider::non_streaming_text("")),
            Duration::from_secs(1),
        );
        let history = [make_user_msg("plan")];

        assert_eq!(
            streaming
                .summarize(&history, "implement")
                .await
                .expect("empty streaming summary"),
            "No summary generated"
        );
        assert_eq!(
            non_streaming
                .summarize(&history, "implement")
                .await
                .expect("empty non-streaming summary"),
            "No summary generated"
        );
    }

    #[tokio::test]
    async fn stream_error_is_propagated_without_non_streaming_fallback() {
        let provider = Arc::new(SummaryTestProvider::streaming(vec![Err(
            LLMError::ProviderError("stream failed".to_string()),
        )]));
        let summarizer = summarizer_with_provider(provider.clone(), Duration::from_secs(1));

        let error = summarizer
            .summarize(&[make_user_msg("plan")], "implement")
            .await
            .expect_err("stream error");

        assert!(error.to_string().contains("stream failed"));
        assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.chat_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn non_streaming_error_is_propagated() {
        let provider = Arc::new(SummaryTestProvider::non_streaming_error(
            LLMError::ProviderError("chat failed".to_string()),
        ));
        let summarizer = summarizer_with_provider(provider, Duration::from_secs(1));

        let error = summarizer
            .summarize(&[make_user_msg("plan")], "implement")
            .await
            .expect_err("chat error");

        assert!(error.to_string().contains("chat failed"));
    }

    #[tokio::test]
    async fn configured_timeout_covers_stream_consumption() {
        let provider = Arc::new(SummaryTestProvider::stalled_stream());
        let summarizer = summarizer_with_provider(provider.clone(), Duration::from_secs(1));

        let error = summarizer
            .summarize(&[make_user_msg("plan")], "implement")
            .await
            .expect_err("stream timeout");

        assert!(error.to_string().contains("timed out after 1 seconds"));
        assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.chat_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn format_conversation_does_not_contain_llm_instructions() {
        let history = vec![
            make_user_msg("Add a login page"),
            make_assistant_msg("I'll create a login component in src/Login.tsx"),
        ];
        let summarizer = test_summarizer();
        let output = summarizer.format_conversation(&history, "Implement login page");

        assert!(output.contains("Delegation objective: Implement login page"));
        assert!(output.contains("[User]: Add a login page"));
        assert!(output.contains("[Planner]: I'll create a login component"));
        // Must NOT contain summarizer LLM instructions
        assert!(
            !output.contains("Write a structured Implementation Brief"),
            "format_conversation should not contain LLM instructions"
        );
        assert!(
            !output.contains("You are a technical brief writer"),
            "format_conversation should not contain LLM role preamble"
        );
    }

    #[test]
    fn prepare_llm_input_contains_instructions() {
        let history = vec![
            make_user_msg("Add a login page"),
            make_assistant_msg("I'll create a login component"),
        ];
        let summarizer = test_summarizer();
        let output = summarizer.prepare_llm_input(&history, "Implement login page");

        // Should contain the conversation content
        assert!(output.contains("[User]: Add a login page"));
        // Should contain LLM instructions
        assert!(output.contains("Write a structured Implementation Brief"));
        assert!(output.contains("You are a technical brief writer"));
    }
}
