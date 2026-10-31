//! AI-powered conversation compaction
//!
//! This module implements the AI compaction layer of the 3-layer compaction system,
//! which generates summaries when context threshold is reached.

use crate::accounting::{AccountingError, DispatchLedger, DispatchMode};
use crate::agent::utils::{genai, render_prompt_for_llm};
use crate::model::{AgentMessage, MessagePart};
use crate::session::pruning::{SimpleTokenEstimator, TokenEstimator};
use anyhow::Result;
use futures_util::{FutureExt, StreamExt};
use querymt::chat::{
    ChatCall, ChatOutput, ChatOutputStatus, ChatRole, ChatStreamAccumulator,
    ChatStreamAccumulatorError, ChatStreamFinish, FinishReason,
};
use querymt::error::LLMError;
use std::sync::Arc;
use std::time::Duration;

/// Default compaction prompt used to generate conversation summaries
pub const COMPACTION_PROMPT: &str = r#"Provide a detailed prompt for continuing our conversation above. Focus on:
- What was done
- What is currently being worked on
- Which files are being modified
- What needs to be done next
- Key user requests, constraints, or preferences that should persist

Be comprehensive but concise. This summary will replace the conversation history."#;

pub(crate) mod codex;

/// Configuration for retry behavior
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum retry attempts
    pub max_retries: usize,
    /// Initial backoff delay in milliseconds
    pub initial_backoff_ms: u64,
    /// Multiplier for exponential backoff
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff_ms: 1000,
            backoff_multiplier: 2.0,
        }
    }
}

/// Result of a compaction operation
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// The generated summary, or a notice when the native checkpoint is opaque.
    pub summary: String,
    /// Original token count before compaction
    pub original_token_count: usize,
    /// Estimated visible size of the replacement context.
    pub summary_token_count: usize,
    /// Canonical provider-native checkpoint, when available.
    pub native_output: Option<ChatOutput>,
}

impl CompactionResult {
    pub(crate) fn take_messages(&mut self, session_id: &str) -> Vec<AgentMessage> {
        let (request, mut summary) = SessionCompaction::create_compaction_messages(
            session_id,
            &self.summary,
            self.original_token_count,
        );
        if let Some(output) = self.native_output.take() {
            if let Some(origin) = &output.provenance {
                summary.source_provider = Some(origin.provider.clone());
                summary.source_model = Some(origin.model.clone());
            }
            // The boundary and encrypted state are committed in one message write.
            summary.parent_message_id = None;
            summary.parts.push(MessagePart::Output { output });
            vec![summary]
        } else {
            vec![request, summary]
        }
    }
}

/// Service for generating AI-powered conversation summaries
#[derive(Clone)]
pub struct SessionCompaction {
    estimator: Arc<dyn TokenEstimator>,
}

impl Default for SessionCompaction {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionCompaction {
    /// Create a new SessionCompaction with default token estimator
    pub fn new() -> Self {
        Self {
            estimator: Arc::new(SimpleTokenEstimator),
        }
    }

    /// Create a new SessionCompaction with custom token estimator
    pub fn with_estimator(estimator: Arc<dyn TokenEstimator>) -> Self {
        Self { estimator }
    }

    /// Process messages and generate a compaction summary
    ///
    /// This method:
    /// 1. Builds a prompt with conversation history
    /// 2. Calls the LLM provider with retry logic
    /// 3. Returns the compaction result
    ///
    /// # Arguments
    ///
    /// * `messages` - The conversation history to summarize
    /// * `provider` - The LLM provider to use for summary generation
    /// * `retry_config` - Configuration for retry behavior
    /// * `ledger` - Where every physical attempt is recorded. Required: the
    ///   initiating session/cycle is explicit and never inferred from
    ///   `messages`. Each retry is a separate attempt, settled before the
    ///   summary is validated so a rejected summary keeps its cost.
    pub async fn process(
        &self,
        messages: &[AgentMessage],
        provider: Arc<dyn querymt::chat::ChatProvider>,
        model: &str,
        retry_config: &RetryConfig,
        max_prompt_bytes: Option<usize>,
        ledger: &DispatchLedger,
    ) -> Result<CompactionResult> {
        self.process_for_target(
            messages,
            provider,
            (model, None),
            retry_config,
            max_prompt_bytes,
            ledger,
        )
        .await
    }

    pub(crate) async fn process_for_target(
        &self,
        messages: &[AgentMessage],
        provider: Arc<dyn querymt::chat::ChatProvider>,
        request_context: (&str, Option<&crate::model::OutputTarget>),
        retry_config: &RetryConfig,
        max_prompt_bytes: Option<usize>,
        ledger: &DispatchLedger,
    ) -> Result<CompactionResult> {
        let (model, target) = request_context;
        let original_token_count = self.estimate_messages_tokens(messages, max_prompt_bytes);
        let chat_messages = self.build_compaction_messages(messages, max_prompt_bytes, target)?;
        let output = self
            .call_with_retry(
                &ChatCall::new(&chat_messages),
                provider,
                model,
                retry_config,
                messages
                    .first()
                    .map(|message| message.session_id.as_str())
                    .filter(|id| !id.is_empty()),
                ledger,
            )
            .await?;
        let summary = Self::summary_from_output(&output)?;
        let summary_token_count = self.estimator.estimate(&summary);
        Ok(CompactionResult {
            summary,
            original_token_count,
            summary_token_count,
            native_output: None,
        })
    }

    fn build_compaction_messages(
        &self,
        messages: &[AgentMessage],
        max_prompt_bytes: Option<usize>,
        target: Option<&crate::model::OutputTarget>,
    ) -> Result<Vec<querymt::chat::ChatMessage>> {
        let mut chat_messages: Vec<querymt::chat::ChatMessage> = messages
            .iter()
            .map(|message| message.to_chat_message_with_output_target(target, max_prompt_bytes))
            .collect::<std::result::Result<_, _>>()?;

        // Add the compaction prompt as a user message
        chat_messages.push(
            querymt::chat::ChatMessage::user()
                .text(COMPACTION_PROMPT)
                .build(),
        );

        Ok(chat_messages)
    }

    /// Call LLM with exponential backoff retry.
    ///
    /// Prefers streaming when the provider supports it.  The streaming path
    /// uses per-chunk timeouts (typically 60 s) instead of a single total
    /// request-response timeout, keeping the connection alive while the model
    /// generates the compaction summary.  This avoids the 10 s libp2p default
    /// timeout that caused compaction failures on remote mesh nodes.
    ///
    /// Mesh timeout errors (remote still processing) are treated as
    /// **non-retriable** to prevent queuing duplicate inference work on the
    /// remote GPU.
    #[tracing::instrument(
        name = "agent.compaction.call_llm",
        skip_all,
        fields(
            otel.name = %format!("chat {model}"),
            otel.kind = "client",
            gen_ai.operation.name = "chat",
            gen_ai.request.model = model,
            gen_ai.conversation.id = session_id,
            session.id = session_id,
            gen_ai.request.stream = provider.capabilities().streaming,
            querymt.compaction = true,
            querymt.compaction.strategy = codex::request_strategy(call).unwrap_or("llm_summary"),
            querymt.compaction.native_trigger_sent = false,
            querymt.compaction.native_checkpoint_received = false,
        )
    )]
    async fn call_with_retry(
        &self,
        call: &ChatCall<'_>,
        provider: Arc<dyn querymt::chat::ChatProvider>,
        model: &str,
        retry_config: &RetryConfig,
        session_id: Option<&str>,
        ledger: &DispatchLedger,
    ) -> Result<ChatOutput> {
        // Provider identity is unavailable on ChatProvider; canonical provenance can supply it.
        genai::finish_reason("error");
        let chat_span = tracing::Span::current();
        let mut timing = genai::StreamTiming::default();
        let mut last_error = None;
        let mut backoff_ms = retry_config.initial_backoff_ms;

        for attempt in 0..=retry_config.max_retries {
            if attempt > 0 {
                log::debug!(
                    "Compaction retry attempt {} after {}ms",
                    attempt,
                    backoff_ms
                );
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms as f64 * retry_config.backoff_multiplier) as u64;
            }

            // Prefer streaming to avoid mesh request-response timeout on long
            // inference.  The streaming path uses per-chunk timeouts instead of
            // a single total-request timeout, keeping the connection alive
            // while the model generates.
            // The start is durable before the request is sent. Failing to
            // record is fatal and never retried: no paid call without a ledger.
            let streaming = provider.capabilities().streaming;
            let attempt_record = ledger
                .operation
                .begin(
                    if streaming {
                        DispatchMode::Streaming
                    } else {
                        DispatchMode::NonStreaming
                    },
                    ledger.basis.clone(),
                )
                .await
                .map_err(accounting_failure)?;

            if codex::request_strategy(call).is_some() {
                chat_span.record("querymt.compaction.native_trigger_sent", true);
            }
            let raw = if streaming {
                Self::call_streaming(call, &provider, &mut timing, &chat_span).await
            } else {
                provider.chat(call).await.map_err(|error| StreamFailure {
                    error,
                    response_started: false,
                    usage: None,
                    output: None,
                })
            };

            // Settle before validation: a refused, empty or incomplete summary
            // still incurred whatever usage it reported.
            let result = match raw {
                Ok(output) => {
                    let native_validation = codex::validate_compaction(call, &output);
                    if native_validation
                        .as_ref()
                        .is_some_and(|validation| validation.is_ok())
                    {
                        chat_span.record("querymt.compaction.native_checkpoint_received", true);
                    }
                    let validated = native_validation
                        .unwrap_or_else(|| Self::summary_from_output(&output).map(|_| ()));
                    match &validated {
                        Ok(_) => attempt_record.settle_output(&output).await,
                        Err(_) => attempt_record.settle_rejected_output(&output, false).await,
                    }
                    .map_err(accounting_failure)?;
                    validated.map(|()| {
                        genai::output(&output);
                        output
                    })
                }
                Err(failure) => {
                    match &failure.output {
                        Some(output) => attempt_record.settle_rejected_output(output, true).await,
                        None => {
                            attempt_record
                                .settle_error(
                                    &failure.error,
                                    failure.response_started,
                                    failure.usage.clone(),
                                )
                                .await
                        }
                    }
                    .map_err(accounting_failure)?;
                    Err(failure.error)
                }
            };

            match result {
                Ok(text) => return Ok(text),
                Err(e) => {
                    log::warn!("Compaction LLM call failed (attempt {})", attempt + 1);
                    // Mesh timeouts mean the remote is still processing.
                    // Retrying would just queue more work on an already-busy
                    // remote GPU — abort immediately.
                    if is_mesh_timeout_error(&e) {
                        genai::llm_error(&e);
                        log::warn!(
                            "Compaction error is a mesh timeout (remote node likely still \
                             processing). Aborting retries to avoid queuing duplicate work."
                        );
                        return Err(anyhow::anyhow!(
                            "Compaction failed: mesh timeout (remote still processing): {}",
                            e
                        ));
                    }
                    last_error = Some(e);
                }
            }
        }

        if let Some(error) = &last_error {
            genai::llm_error(error);
        }
        Err(anyhow::anyhow!(
            "Compaction failed after {} retries: {:?}",
            retry_config.max_retries,
            last_error
        ))
    }

    /// Collect legacy or structured output, requiring a successful stream completion.
    ///
    /// Failures keep what accounting needs: whether any response bytes were
    /// seen, the usage observed so far, and any partial output.
    async fn call_streaming(
        call: &ChatCall<'_>,
        provider: &Arc<dyn querymt::chat::ChatProvider>,
        timing: &mut genai::StreamTiming,
        chat_span: &tracing::Span,
    ) -> std::result::Result<ChatOutput, StreamFailure> {
        timing.start();
        let mut stream = provider
            .chat_stream(call)
            .await
            .map_err(|error| StreamFailure {
                error,
                response_started: false,
                usage: None,
                output: None,
            })?;
        timing.headers_received(chat_span);
        let mut accumulator = ChatStreamAccumulator::new();
        let mut response_started = false;
        let mut terminal_seen = false;
        loop {
            timing.wait_begin();
            let next = stream.next().await;
            timing.wait_end();
            let Some(chunk) = next else { break };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    return Err(StreamFailure {
                        error,
                        response_started,
                        usage: accumulator.output().usage,
                        output: None,
                    });
                }
            };
            response_started = true;
            timing.chunk_received(chat_span, genai::chunk_commits_output(&chunk));
            if let Err(error) = accumulator.push(&chunk) {
                return Err(StreamFailure {
                    error: LLMError::GenericError(format!("Invalid compaction stream: {error}")),
                    response_started,
                    usage: accumulator.output().usage,
                    output: None,
                });
            }
            if querymt::chat::chunk_is_terminal(&chunk) {
                terminal_seen = true;
                break;
            }
        }
        if !terminal_seen {
            return Err(StreamFailure {
                error: LLMError::GenericError(
                    "Compaction stream ended without a terminal event".into(),
                ),
                response_started,
                usage: accumulator.output().usage,
                output: None,
            });
        }
        timing.finish(chat_span);
        // Only collect already-buffered usage; post-terminal events must bypass validation.
        let mut trailing: Option<querymt::Usage> = None;
        while let Some(Some(Ok(querymt::chat::StreamChunk::Usage(extra)))) =
            stream.next().now_or_never()
        {
            trailing = Some(match trailing.take() {
                Some(previous) => previous.merge_max(extra),
                None => extra,
            });
        }
        let mut merge = |mut output: ChatOutput| {
            if let Some(extra) = trailing.take() {
                output.usage = Some(match output.usage.take() {
                    Some(previous) => previous.merge_max(extra),
                    None => extra,
                });
            }
            output
        };
        let invalid = |error: ChatStreamAccumulatorError| {
            LLMError::GenericError(format!("Invalid compaction stream: {error}"))
        };
        match accumulator.finish() {
            ChatStreamFinish::Completed(output) => Ok(merge(output)),
            ChatStreamFinish::Incomplete { output, detail } => {
                let output = merge(output);
                Err(StreamFailure {
                    error: invalid(ChatStreamAccumulatorError::IncompleteResponse(detail)),
                    response_started,
                    usage: output.usage.clone(),
                    output: Some(Box::new(output)),
                })
            }
            ChatStreamFinish::Failed { output, error } => {
                let output = merge(output);
                Err(StreamFailure {
                    error: invalid(error),
                    response_started,
                    usage: output.usage.clone(),
                    output: Some(Box::new(output)),
                })
            }
        }
    }

    /// Partial output and tool calls cannot replace a conversation with a usable summary.
    fn summary_from_output(output: &ChatOutput) -> std::result::Result<String, LLMError> {
        if output
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
            return Err(LLMError::GenericError(format!(
                "Compaction response did not complete successfully (status={:?}, finish_reason={:?})",
                output.status, output.finish_reason
            )));
        }
        let summary = output.text().unwrap_or_default();
        Self::validate_summary(&summary)?;
        Ok(summary)
    }

    /// Validate provider and hook summaries before they can create a history boundary.
    pub(crate) fn validate_summary(summary: &str) -> std::result::Result<(), LLMError> {
        if summary.trim().is_empty() {
            return Err(LLMError::GenericError(
                "Compaction produced an empty summary".into(),
            ));
        }
        Ok(())
    }

    /// Estimate token count for a list of messages
    pub(crate) fn estimate_messages_tokens(
        &self,
        messages: &[AgentMessage],
        max_prompt_bytes: Option<usize>,
    ) -> usize {
        messages
            .iter()
            .map(|m| {
                m.parts
                    .iter()
                    .map(|p| match p {
                        MessagePart::Text { content }
                        | MessagePart::HookContext { content, .. } => {
                            self.estimator.estimate(content)
                        }
                        MessagePart::Prompt { blocks } | MessagePart::Steering { blocks, .. } => {
                            self.estimator
                                .estimate(&render_prompt_for_llm(blocks, max_prompt_bytes))
                        }
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

    /// Create a compaction message pair to be stored in history.
    ///
    /// Returns a (user_request, assistant_summary) tuple that forms a natural
    /// user→assistant exchange after context compaction. The user message asks
    /// "Summarize our conversation so far." and the assistant message contains
    /// the actual summary.
    pub fn create_compaction_messages(
        session_id: &str,
        summary: &str,
        original_token_count: usize,
    ) -> (AgentMessage, AgentMessage) {
        let request_id = uuid::Uuid::now_v7().to_string();
        let summary_id = uuid::Uuid::now_v7().to_string();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();

        let request_msg = AgentMessage {
            id: request_id.clone(),
            session_id: session_id.to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::CompactionRequest {
                original_token_count,
            }],
            created_at: now,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };

        let summary_msg = AgentMessage {
            id: summary_id,
            session_id: session_id.to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Compaction {
                summary: summary.to_string(),
                original_token_count,
            }],
            created_at: now,
            parent_message_id: Some(request_id),
            source_provider: None,
            source_model: None,
        };

        (request_msg, summary_msg)
    }
}

/// A failed physical attempt, with what accounting needs to settle it.
struct StreamFailure {
    error: LLMError,
    /// True once any response bytes arrived: billing is then never
    /// "confirmed not billable".
    response_started: bool,
    /// Usage observed before the failure.
    usage: Option<querymt::Usage>,
    /// Partial canonical output, when the stream reached finalization.
    output: Option<Box<ChatOutput>>,
}

fn accounting_failure(error: AccountingError) -> anyhow::Error {
    anyhow::anyhow!("Compaction accounting failure: {error}")
}

/// Check if an LLM error indicates a mesh timeout (remote node still processing).
///
/// These errors should **not** be retried because the remote GPU is still busy
/// with the previous request.  Retrying just queues additional inference work,
/// wasting compute and potentially causing cascading timeouts.
///
/// Detection matches on the typed `LLMError::Transport { kind: Timeout, .. }`
/// variant which is produced by `MeshChatProvider::chat_with_tools` when
/// kameo's `RemoteSendError::Timeout` fires, or when a mesh stream's
/// reconnect grace period expires.
fn is_mesh_timeout_error(e: &querymt::error::LLMError) -> bool {
    matches!(
        e,
        querymt::error::LLMError::Transport {
            kind: querymt::error::TransportErrorKind::Timeout,
            ..
        }
    )
}

/// Filter messages to return only the "effective" history after the last compaction.
///
/// Text compaction keeps its request/summary pair and subsequent messages.
/// Native Codex checkpoints prepend their retained user context to that suffix.
///
/// # Arguments
///
/// * `messages` - The full conversation history
///
/// # Returns
///
/// The latest checkpoint's replacement history plus subsequent messages,
/// or all messages if no compaction exists.
pub fn filter_to_effective_history(mut messages: Vec<AgentMessage>) -> Vec<AgentMessage> {
    let last_compaction_idx = messages.iter().rposition(|message| {
        message
            .parts
            .iter()
            .any(|part| matches!(part, MessagePart::CompactionRequest { .. }))
            || codex::retained_messages(message).is_some()
    });

    let filtered: Vec<AgentMessage> = match last_compaction_idx {
        Some(idx) => {
            let mut retained = codex::retained_messages(&messages[idx]).unwrap_or_default();
            for message in &mut retained {
                message.session_id.clone_from(&messages[idx].session_id);
            }
            retained.extend(messages.split_off(idx));
            retained
        }
        None => messages,
    };

    // Filter out messages that only contain snapshot metadata parts
    // These are for undo/redo tracking and should not be sent to the LLM
    // Keeping them creates empty messages that break tool_use -> tool_result sequencing
    let without_snapshots = filtered
        .into_iter()
        .filter(|m| {
            m.parts.iter().any(|p| {
                !matches!(
                    p,
                    MessagePart::TurnSnapshotStart { .. } | MessagePart::TurnSnapshotPatch { .. }
                )
            })
        })
        .collect::<Vec<_>>();

    // Safety net: merge consecutive User messages that each contain a
    // tool_result part.  Legacy sessions stored one message per tool result
    // which violates the LLM API contract that all tool_results for a batch
    // of tool_use calls appear in a single user message.
    merge_consecutive_tool_result_messages(without_snapshots)
}

/// Merge runs of consecutive `User` messages where **every** message in the
/// run contains at least one `ToolResult` part.  The parts from subsequent
/// messages are folded into the first message of the run.
///
/// This repairs data written by older versions of `store_all_tool_results`
/// which created one message per tool result instead of batching them.
pub fn merge_consecutive_tool_result_messages(messages: Vec<AgentMessage>) -> Vec<AgentMessage> {
    if messages.len() < 2 {
        return messages;
    }

    fn is_user_tool_result(m: &AgentMessage) -> bool {
        m.role == ChatRole::User
            && m.parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. }))
    }

    let mut result: Vec<AgentMessage> = Vec::with_capacity(messages.len());

    for msg in messages {
        let dominated_by_tool_result = is_user_tool_result(&msg);
        let should_merge =
            dominated_by_tool_result && result.last().is_some_and(is_user_tool_result);

        if should_merge {
            // Safe: we just checked last() is Some.
            let prev = result.last_mut().unwrap();
            prev.parts.extend(msg.parts);
        } else {
            result.push(msg);
        }
    }

    result
}

/// Check if messages contain a compaction pair
pub fn has_compaction(messages: &[AgentMessage]) -> bool {
    messages.iter().any(|m| {
        m.parts.iter().any(|p| {
            matches!(
                p,
                MessagePart::CompactionRequest { .. } | MessagePart::Compaction { .. }
            )
        })
    })
}

/// Get the most recent compaction summary if it exists
pub fn get_last_compaction(messages: &[AgentMessage]) -> Option<&MessagePart> {
    for message in messages.iter().rev() {
        for part in &message.parts {
            if matches!(part, MessagePart::Compaction { .. }) {
                return Some(part);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::OutputTarget;
    use crate::test_utils::mocks::MockCompactionProvider;
    use querymt::chat::{
        ChatMessageItem, ChatMessagePart, ChatMessagePartDelta, ChatOpaqueItem, ChatOutputItem,
        ChatOutputProvenance, StreamChunk, StructuredStreamEvent,
    };
    use querymt::error::LLMError;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A ledger over a fresh in-memory journal with no pricing.
    async fn test_ledger() -> DispatchLedger {
        ledger_with(querymt::providers::BillingClass::Unknown, None)
            .await
            .0
    }

    async fn ledger_with(
        billing: querymt::providers::BillingClass,
        pricing: Option<querymt::providers::ModelPricing>,
    ) -> (DispatchLedger, Arc<crate::accounting::SessionAccounting>) {
        use crate::session::backend::StorageBackend;
        let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap();
        let journal = storage.event_journal();
        let accounting =
            crate::accounting::SessionAccounting::hydrate("compaction-session", journal.clone())
                .await
                .unwrap();
        let stream = crate::session_event_stream::SessionEventStream::new(
            "compaction-session",
            journal,
            Arc::new(crate::event_fanout::EventFanout::new()),
        );
        let recorder = Arc::new(crate::accounting::AttemptRecorder::with_projection(
            stream,
            accounting.clone(),
        ));
        let cycle = crate::accounting::CycleAccounting::with_id(accounting.clone(), "cycle-1");
        let operation = crate::accounting::AttemptOperation::new(
            recorder,
            &cycle,
            crate::accounting::AttemptPurpose::Compaction,
            "p",
            "model",
        );
        (
            DispatchLedger {
                operation: Arc::new(operation),
                basis: crate::session::provider::BillingBasis { billing, pricing },
            },
            accounting,
        )
    }

    #[tokio::test]
    async fn genai_compaction_stream_retry_records_one_canonical_output_without_content() {
        use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
        use opentelemetry::{
            Array, Value,
            trace::{SpanKind, Status},
        };
        use querymt::chat::ChatOutputProvenance;
        use tracing::Instrument;

        for session_id in [None, Some("compaction-session"), Some("")] {
            let mut chunks = structured_summary("SECRET_RESPONSE", true);
            if let StreamChunk::Structured(StructuredStreamEvent::ResponseMetadata {
                provenance,
                ..
            }) = &mut chunks[0]
            {
                *provenance = Some(ChatOutputProvenance {
                    provider: "anthropic".into(),
                    model: "summary-model".into(),
                    endpoint: "SECRET_ENDPOINT".into(),
                    protocol: "messages".into(),
                });
            }
            if let StreamChunk::Structured(StructuredStreamEvent::ResponseTerminal {
                usage, ..
            }) = chunks.last_mut().unwrap()
            {
                *usage = Some(querymt::Usage {
                    input_tokens: 12,
                    output_tokens: 20,
                    cache_read: 30,
                    cache_write: 40,
                    reasoning_tokens: 5,
                    cache_write_tiers: None,
                });
            }
            let provider = ScriptedProvider::new(
                true,
                vec![
                    Attempt::Stream(vec![Err(LLMError::AuthError("SECRET_ERROR".into()))]),
                    Attempt::Stream(chunks.into_iter().map(Ok).collect()),
                ],
            );
            let history: Vec<_> = session_id
                .into_iter()
                .map(|id| MessageFixture::user_message("m1", id, "SECRET_PROMPT"))
                .collect();
            let (result, spans) = capture(async {
                SessionCompaction::new()
                    .process(
                        &history,
                        provider.clone(),
                        "model",
                        &retry_config(1),
                        None,
                        &test_ledger().await,
                    )
                    .instrument(tracing::info_span!(
                        "unrelated-parent",
                        session.id = "unrelated-session"
                    ))
                    .await
            })
            .await;
            assert_eq!(result.unwrap().summary, "SECRET_RESPONSE");
            assert_eq!(provider.calls.load(Ordering::Relaxed), 2);
            assert_eq!(
                spans
                    .iter()
                    .filter(|span| span.name == "chat model")
                    .count(),
                1
            );
            let chat = spans.iter().find(|span| span.name == "chat model").unwrap();
            assert_eq!(chat.name, "chat model");
            assert_eq!(
                attr(chat, "querymt.compaction.strategy"),
                Some(&Value::from("llm_summary"))
            );
            assert_eq!(
                attr(chat, "querymt.compaction.native_trigger_sent"),
                Some(&Value::Bool(false))
            );
            assert_eq!(
                attr(chat, "querymt.compaction.native_checkpoint_received"),
                Some(&Value::Bool(false))
            );
            assert_eq!(
                attr(chat, "gen_ai.request.model"),
                Some(&Value::from("model"))
            );
            assert_eq!(
                attr(chat, "gen_ai.response.model"),
                Some(&Value::from("summary-model"))
            );
            assert_eq!(chat.span_kind, SpanKind::Client);
            assert_eq!(chat.status, Status::Unset);
            assert_eq!(
                attr(chat, "gen_ai.usage.input_tokens"),
                Some(&Value::I64(82))
            );
            assert_eq!(
                attr(chat, "gen_ai.usage.output_tokens"),
                Some(&Value::I64(25))
            );
            assert_eq!(
                attr(chat, "gen_ai.usage.reasoning.output_tokens"),
                Some(&Value::I64(5))
            );
            assert_eq!(
                attr(chat, "gen_ai.usage.reasoning_tokens"),
                Some(&Value::I64(5))
            );
            assert!(attr(chat, "gen_ai.request.reasoning.level").is_none());
            assert_eq!(
                attr(chat, "gen_ai.request.stream"),
                Some(&Value::Bool(true))
            );
            assert_eq!(
                attr(chat, "gen_ai.response.finish_reasons"),
                Some(&Value::Array(Array::String(vec!["stop".into()])))
            );
            assert_eq!(
                attr(chat, "gen_ai.conversation.id"),
                session_id
                    .filter(|id| !id.is_empty())
                    .map(Value::from)
                    .as_ref()
            );
            assert_eq!(
                attr(chat, "session.id"),
                attr(chat, "gen_ai.conversation.id")
            );
            assert!(attr(chat, "error.type").is_none());
            assert_private(&spans);
        }
    }

    #[tokio::test]
    async fn genai_compaction_drains_buffered_usage_without_waiting_or_revalidating() {
        use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
        use opentelemetry::Value;

        for structured in [false, true] {
            for pending_tail in [false, true] {
                let initial = querymt::Usage {
                    input_tokens: 12,
                    output_tokens: 20,
                    ..Default::default()
                };
                let mut chunks = if structured {
                    let mut chunks = structured_summary("SECRET_RESPONSE", true);
                    if let StreamChunk::Structured(StructuredStreamEvent::ResponseTerminal {
                        usage,
                        ..
                    }) = chunks.last_mut().unwrap()
                    {
                        *usage = Some(initial);
                    }
                    chunks
                } else {
                    vec![
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
                        ..Default::default()
                    }),
                ]);
                if !pending_tail {
                    // A post-terminal compatibility projection cannot change the canonical output.
                    chunks.push(StreamChunk::Text("SECRET_ARGUMENT".into()));
                }
                let chunks = chunks.into_iter().map(Ok).collect();
                let attempt = if pending_tail {
                    Attempt::StreamWithPendingTail(chunks)
                } else {
                    Attempt::Stream(chunks)
                };
                let provider = ScriptedProvider::new(true, vec![attempt]);
                let (result, spans) = capture(tokio::time::timeout(
                    Duration::from_secs(1),
                    process_script(provider, 0),
                ))
                .await;
                assert_eq!(
                    result
                        .expect("must not await the pending tail")
                        .unwrap()
                        .summary,
                    "SECRET_RESPONSE"
                );
                assert_eq!(spans.len(), 1);
                assert_eq!(
                    attr(&spans[0], "gen_ai.usage.input_tokens"),
                    Some(&Value::I64(37))
                );
                assert_eq!(
                    attr(&spans[0], "gen_ai.usage.output_tokens"),
                    Some(&Value::I64(40))
                );
                assert_private(&spans);
            }
        }
    }

    #[tokio::test]
    async fn genai_compaction_first_chunk_is_logical_across_retry_helper_spans() {
        use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
        use opentelemetry::Value;
        for metadata_first in [false, true] {
            let error = || LLMError::AuthError("SECRET_ERROR".into());
            let initial = if metadata_first {
                Attempt::Stream(vec![
                    Ok(StreamChunk::Usage(Default::default())),
                    Err(error()),
                ])
            } else {
                Attempt::SetupError(error())
            };
            let mut provider = ScriptedProvider::new(
                true,
                vec![
                    initial,
                    Attempt::Stream(
                        structured_summary("SECRET_RESPONSE", true)
                            .into_iter()
                            .map(Ok)
                            .collect(),
                    ),
                ],
            );
            Arc::get_mut(&mut provider).unwrap().delay = Duration::from_millis(20);
            let history = [MessageFixture::user_message(
                "m1",
                "session",
                "SECRET_PROMPT",
            )];
            let (result, spans) = capture(SessionCompaction::new().process(
                &history,
                provider.clone(),
                "model",
                &RetryConfig {
                    max_retries: 1,
                    initial_backoff_ms: 60,
                    backoff_multiplier: 1.0,
                },
                None,
                &test_ledger().await,
            ))
            .await;
            assert!(result.is_ok());
            let chat = spans.iter().find(|span| span.name == "chat model").unwrap();
            let expected = provider.clock.lock().unwrap().1.unwrap();
            assert!(
                matches!(attr(chat, "gen_ai.response.time_to_first_chunk"), Some(Value::F64(actual)) if (actual - expected).abs() < 0.03)
            );
            if !metadata_first {
                assert!(expected >= 0.1);
            }
            assert_eq!(
                chat.attributes
                    .iter()
                    .filter(|attr| attr.key.as_str() == "gen_ai.response.time_to_first_chunk")
                    .count(),
                1
            );
            assert!(attr(chat, "gen_ai.agent.id").is_none());
            for span in spans
                .iter()
                .filter(|span| span.span_context.span_id() != chat.span_context.span_id())
            {
                assert!(attr(span, "gen_ai.response.time_to_first_chunk").is_none());
            }
            assert_private(&spans);
        }
    }

    #[tokio::test]
    async fn genai_compaction_failed_stream_records_sanitized_terminal_error() {
        use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
        use opentelemetry::{Value, trace::Status};
        let provider = ScriptedProvider::new(
            true,
            vec![Attempt::Stream(vec![Err(LLMError::AuthError(
                "SECRET_ERROR".into(),
            ))])],
        );
        let (result, spans) = capture(process_script(provider, 0)).await;
        assert!(result.is_err());
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].status, Status::error(""));
        assert!(attr(&spans[0], "gen_ai.response.time_to_first_chunk").is_none());
        assert_eq!(
            attr(&spans[0], "error.type"),
            Some(&Value::from("authentication"))
        );
        assert_private(&spans);
    }

    enum Attempt {
        Output(Box<ChatOutput>),
        Stream(Vec<std::result::Result<StreamChunk, LLMError>>),
        StreamWithPendingTail(Vec<std::result::Result<StreamChunk, LLMError>>),
        SetupError(LLMError),
    }

    impl Attempt {
        fn output(output: ChatOutput) -> Self {
            Self::Output(Box::new(output))
        }
    }

    struct ScriptedProvider {
        streaming: bool,
        attempts: tokio::sync::Mutex<VecDeque<Attempt>>,
        requests: std::sync::Mutex<Vec<Vec<querymt::chat::ChatMessage>>>,
        offered_tools: std::sync::Mutex<Vec<Vec<querymt::chat::Tool>>>,
        calls: AtomicUsize,
        delay: Duration,
        clock: Arc<std::sync::Mutex<(Option<std::time::Instant>, Option<f64>)>>,
    }

    impl ScriptedProvider {
        fn new(streaming: bool, attempts: Vec<Attempt>) -> Arc<Self> {
            Arc::new(Self {
                streaming,
                attempts: tokio::sync::Mutex::new(attempts.into()),
                requests: Default::default(),
                offered_tools: Default::default(),
                calls: AtomicUsize::new(0),
                delay: Duration::ZERO,
                clock: Default::default(),
            })
        }

        async fn next(&self) -> Attempt {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.attempts
                .lock()
                .await
                .pop_front()
                .expect("unexpected provider retry")
        }
    }

    #[async_trait::async_trait]
    impl querymt::chat::ChatProvider for ScriptedProvider {
        fn capabilities(&self) -> querymt::chat::ChatCapabilities {
            querymt::chat::ChatCapabilities::NONE.with_streaming(self.streaming)
        }

        async fn chat(&self, call: &ChatCall<'_>) -> std::result::Result<ChatOutput, LLMError> {
            self.requests.lock().unwrap().push(call.messages.to_vec());
            self.offered_tools
                .lock()
                .unwrap()
                .push(call.tools.unwrap_or_default().to_vec());
            let Attempt::Output(output) = self.next().await else {
                panic!("expected non-streaming call")
            };
            Ok(*output)
        }

        async fn chat_stream(
            &self,
            call: &ChatCall<'_>,
        ) -> std::result::Result<querymt::chat::ChatStream, LLMError> {
            self.requests.lock().unwrap().push(call.messages.to_vec());
            self.offered_tools
                .lock()
                .unwrap()
                .push(call.tools.unwrap_or_default().to_vec());
            self.clock
                .lock()
                .unwrap()
                .0
                .get_or_insert_with(std::time::Instant::now);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            let timed = |chunks| {
                let mut delay = self.delay;
                let clock = self.clock.clone();
                futures_util::stream::iter(chunks).then(
                    move |chunk: std::result::Result<StreamChunk, LLMError>| {
                        let delay = std::mem::take(&mut delay);
                        let clock = clock.clone();
                        async move {
                            if !delay.is_zero() {
                                tokio::time::sleep(delay).await;
                            }
                            if chunk.is_ok() {
                                let mut clock = clock.lock().unwrap();
                                if clock.1.is_none() {
                                    clock.1 = Some(clock.0.unwrap().elapsed().as_secs_f64());
                                }
                            }
                            chunk
                        }
                    },
                )
            };
            match self.next().await {
                Attempt::Stream(chunks) => Ok(Box::pin(timed(chunks))),
                Attempt::StreamWithPendingTail(chunks) => Ok(Box::pin(
                    timed(chunks).chain(futures_util::stream::pending()),
                )),
                Attempt::SetupError(error) => Err(error),
                Attempt::Output(_) => panic!("expected streaming call"),
            }
        }
    }

    fn retry_config(max_retries: usize) -> RetryConfig {
        RetryConfig {
            max_retries,
            initial_backoff_ms: 0,
            backoff_multiplier: 1.0,
        }
    }

    fn message_item(text: &str) -> ChatOutputItem {
        ChatOutputItem::Message(ChatMessageItem {
            id: Some("msg-summary".into()),
            role: ChatRole::Assistant,
            phase: None,
            status: Some(ChatOutputStatus::Completed),
            parts: vec![ChatMessagePart::Text {
                text: text.into(),
                annotations: vec![],
                extensions: Default::default(),
            }],
            extensions: Default::default(),
        })
    }

    fn structured_summary(text: &str, with_deltas: bool) -> Vec<StreamChunk> {
        let mut chunks = vec![StreamChunk::Structured(
            StructuredStreamEvent::ResponseMetadata {
                response_id: Some("response-summary".into()),
                status: Some(ChatOutputStatus::InProgress),
                usage: None,
                finish_reason: None,
                provenance: None,
            },
        )];
        if with_deltas {
            let mut item = message_item("");
            if let ChatOutputItem::Message(message) = &mut item {
                message.status = Some(ChatOutputStatus::InProgress);
            }
            chunks.push(StreamChunk::Structured(
                StructuredStreamEvent::ItemStarted {
                    output_index: 0,
                    item,
                },
            ));
            chunks.push(StreamChunk::Structured(
                StructuredStreamEvent::MessagePartDelta {
                    output_index: 0,
                    content_index: 0,
                    delta: ChatMessagePartDelta::Text { delta: text.into() },
                },
            ));
            // Compatibility projections must not duplicate the canonical summary.
            chunks.push(StreamChunk::Text(text.into()));
        }
        chunks.push(StreamChunk::Structured(
            StructuredStreamEvent::ItemCompleted {
                output_index: 0,
                item: message_item(text),
            },
        ));
        chunks.push(StreamChunk::Structured(
            StructuredStreamEvent::ResponseTerminal {
                status: ChatOutputStatus::Completed,
                usage: None,
                finish_reason: Some(FinishReason::Stop),
                detail: None,
            },
        ));
        chunks
    }

    #[test]
    fn handoff_projection_preserves_only_compatible_checkpoint_state() {
        let mut message = AgentMessage::new("handoff".into(), ChatRole::Assistant);
        message.parts.push(MessagePart::Output {
            output: codex_compaction_output("handoff-secret"),
        });
        let target = codex_target();
        let compaction = SessionCompaction::new();
        let native = compaction
            .build_compaction_messages(&[message.clone()], None, Some(&target))
            .unwrap();
        assert!(
            serde_json::to_string(&native)
                .unwrap()
                .contains("handoff-secret")
        );
        let other = OutputTarget::new("xai", "grok");
        let portable = compaction
            .build_compaction_messages(&[message], None, Some(&other))
            .unwrap();
        let text = serde_json::to_string(&portable).unwrap();
        assert!(!text.contains("handoff-secret"));
        assert!(text.contains("visible reasoning summary"));
    }

    fn codex_target() -> OutputTarget {
        OutputTarget::new("codex", "codex-mini-latest")
            .with_protocol("responses")
            .with_endpoint("https://chatgpt.com/backend-api/codex/responses")
    }

    fn codex_compaction_output(state: &str) -> ChatOutput {
        use querymt::chat::{ChatReasoningItem, ChatReasoningPart};
        let target = codex_target();
        ChatOutput {
            status: Some(ChatOutputStatus::Completed),
            finish_reason: Some(FinishReason::Stop),
            provenance: Some(ChatOutputProvenance {
                provider: target.provider,
                protocol: target.protocol,
                model: target.model,
                endpoint: target.endpoint,
            }),
            items: vec![
                ChatOutputItem::Reasoning(ChatReasoningItem {
                    id: Some("reasoning-1".into()),
                    summary: vec![ChatReasoningPart::text("visible reasoning summary")],
                    encrypted_content: Some("reasoning-state".into()),
                    content: Vec::new(),
                    signature: None,
                    status: None,
                    extensions: Default::default(),
                }),
                ChatOutputItem::Opaque(ChatOpaqueItem {
                    original_type: "compaction".into(),
                    payload: serde_json::json!({"type":"compaction", "encrypted_content":state}),
                }),
            ],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn codex_native_checkpoint_survives_reload_switch_back_and_fork_until_out_of_range() {
        use crate::session::{sqlite_storage::SqliteStorage, store::SessionStore};
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let session = storage
            .create_session(None, None, None, None)
            .await
            .unwrap();
        let target = codex_target();
        let user = MessageFixture::user_message("user-1", &session.public_id, "keep my request");
        let mut reasoning = MessageFixture::assistant_message(
            "reasoning-1",
            &session.public_id,
            "ignored projection",
        );
        reasoning.parts = vec![MessagePart::Output {
            output: codex_compaction_output("prior-state"),
        }];
        let mut mixed =
            MessageFixture::user_message("mixed-1", &session.public_id, "retain steering too");
        mixed.parts.push(MessagePart::ToolResult {
            call_id: "old-call".into(),
            tool_name: None,
            tool_arguments: None,
            content: vec![querymt::chat::ToolResultPart::text("old tool result")],
            is_error: false,
            compacted_at: None,
        });
        let history = vec![user, reasoning, mixed];
        for message in &history {
            storage
                .add_message(&session.public_id, message.clone())
                .await
                .unwrap();
        }
        let output = codex_compaction_output("checkpoint-state");
        let mut chunks = vec![Ok(StreamChunk::Structured(
            StructuredStreamEvent::ResponseMetadata {
                response_id: Some("compact-1".into()),
                status: Some(ChatOutputStatus::InProgress),
                usage: None,
                finish_reason: None,
                provenance: output.provenance.clone(),
            },
        ))];
        for (output_index, item) in output.items.iter().cloned().enumerate() {
            chunks.push(Ok(StreamChunk::Structured(
                StructuredStreamEvent::ItemCompleted { output_index, item },
            )));
        }
        chunks.push(Ok(StreamChunk::Structured(
            StructuredStreamEvent::ResponseTerminal {
                status: ChatOutputStatus::Completed,
                usage: None,
                finish_reason: Some(FinishReason::Stop),
                detail: None,
            },
        )));
        let provider = ScriptedProvider::new(true, vec![Attempt::Stream(chunks)]);
        let tools = [querymt::chat::Tool {
            tool_type: "function".into(),
            function: querymt::chat::FunctionTool {
                name: "read_tool".into(),
                description: "Read a file".into(),
                parameters: serde_json::json!({"type":"object", "properties":{}}),
                strict: None,
            },
        }];
        use crate::test_utils::helpers::genai_trace::{attr, capture};
        use opentelemetry::Value;
        let (result, spans) = capture(async {
            SessionCompaction::new()
                .process_codex(
                    &history,
                    provider.clone(),
                    codex::CompactionContext {
                        target: &target,
                        tools: &tools,
                        threshold: None,
                    },
                    &retry_config(0),
                    None,
                    &test_ledger().await,
                )
                .await
        })
        .await;
        let mut result = result.unwrap();
        let chat = spans
            .iter()
            .find(|span| span.name == "chat codex-mini-latest")
            .unwrap();
        assert_eq!(
            attr(chat, "querymt.compaction.strategy"),
            Some(&Value::from("codex_native"))
        );
        assert_eq!(
            attr(chat, "querymt.compaction.native_trigger_sent"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            attr(chat, "querymt.compaction.native_checkpoint_received"),
            Some(&Value::Bool(true))
        );
        {
            assert_eq!(
                provider.offered_tools.lock().unwrap()[0][0].function.name,
                "read_tool"
            );
            let requests = provider.requests.lock().unwrap();
            let request = &requests[0];
            let encoded = serde_json::to_string(request).unwrap();
            assert!(encoded.contains("visible reasoning summary"));
            assert!(encoded.contains("reasoning-state"));
            assert!(!encoded.contains(COMPACTION_PROMPT));
            assert!(
                matches!(&request.last().unwrap().output().unwrap().items[0],
                ChatOutputItem::Opaque(item) if item.original_type == "compaction_trigger")
            );
        }
        let checkpoint = result.take_messages(&session.public_id);
        assert_eq!(
            checkpoint.len(),
            1,
            "native boundary and payload use one atomic message write"
        );
        let checkpoint_id = checkpoint[0].id.clone();
        storage
            .add_message(&session.public_id, checkpoint.into_iter().next().unwrap())
            .await
            .unwrap();
        let reloaded = storage.get_history(&session.public_id).await.unwrap();
        let effective = filter_to_effective_history(reloaded);
        assert_eq!(
            effective.len(),
            3,
            "two retained user contexts plus the checkpoint"
        );
        assert!(effective[..2].iter().all(|message| {
            message.role == ChatRole::User
                && !message
                    .parts
                    .iter()
                    .any(|part| matches!(part, MessagePart::ToolResult { .. }))
        }));
        let checkpoint = effective.last().unwrap();
        let before = serde_json::to_value(checkpoint).unwrap();
        for other in [
            OutputTarget::new("anthropic", "claude"),
            OutputTarget::new("openai", "codex-mini-latest")
                .with_protocol("responses")
                .with_endpoint(&target.endpoint),
            OutputTarget::new("codex", "different-model")
                .with_protocol("responses")
                .with_endpoint(&target.endpoint),
            target.clone().with_endpoint("different-endpoint"),
        ] {
            let portable = checkpoint
                .to_chat_message_with_output_target(Some(&other), None)
                .unwrap();
            let encoded = serde_json::to_string(&portable).unwrap();
            assert!(!encoded.contains("checkpoint-state"));
            assert!(!encoded.contains("reasoning-state"));
            assert!(encoded.contains("visible reasoning summary"));
            assert!(
                portable.portable_input_parts().iter().any(|part| part
                    .as_text()
                    .is_some_and(|text| text.contains("visible reasoning summary"))),
                "providers that ignore unsigned reasoning must still receive the summary as text"
            );
        }
        let back = checkpoint
            .to_chat_message_with_output_target(Some(&target), None)
            .unwrap();
        let encoded = serde_json::to_string(&back).unwrap();
        assert!(encoded.contains("checkpoint-state"));
        assert!(encoded.contains("reasoning-state"));
        assert!(encoded.contains("visible reasoning summary"));
        assert_eq!(serde_json::to_value(checkpoint).unwrap(), before);

        let fork = storage
            .fork_session(
                &session.public_id,
                &checkpoint_id,
                crate::session::domain::ForkOrigin::User,
            )
            .await
            .unwrap();
        let fork_history = filter_to_effective_history(storage.get_history(&fork).await.unwrap());
        assert_eq!(fork_history.len(), 3);
        assert!(
            fork_history
                .iter()
                .all(|message| message.session_id == fork)
        );

        let mut compact_only = codex_compaction_output("new-checkpoint");
        compact_only.items.remove(0);
        let provider = ScriptedProvider::new(false, vec![Attempt::output(compact_only)]);
        let mut next = SessionCompaction::new()
            .process_codex(
                &effective,
                provider,
                codex::CompactionContext {
                    target: &target,
                    tools: &[],
                    threshold: None,
                },
                &retry_config(0),
                None,
                &test_ledger().await,
            )
            .await
            .unwrap();
        for message in next.take_messages(&session.public_id) {
            storage
                .add_message(&session.public_id, message)
                .await
                .unwrap();
        }
        let effective =
            filter_to_effective_history(storage.get_history(&session.public_id).await.unwrap());
        assert_eq!(
            effective.len(),
            3,
            "repeated compaction does not duplicate retained users"
        );
        let encoded = serde_json::to_string(&effective).unwrap();
        assert!(encoded.contains("new-checkpoint"));
        assert!(!encoded.contains("checkpoint-state"));
        let portable = effective
            .last()
            .unwrap()
            .to_chat_message_with_output_target(
                Some(&OutputTarget::new("anthropic", "claude")),
                None,
            )
            .unwrap();
        assert!(
            portable
                .output()
                .unwrap()
                .text()
                .unwrap()
                .contains("unavailable for this provider")
        );
        assert!(
            !serde_json::to_string(&portable)
                .unwrap()
                .contains("new-checkpoint")
        );
        let back = effective
            .last()
            .unwrap()
            .to_chat_message_with_output_target(Some(&target), None)
            .unwrap();
        assert!(
            serde_json::to_string(&back)
                .unwrap()
                .contains("new-checkpoint")
        );
        let (request, summary) = SessionCompaction::create_compaction_messages(
            &session.public_id,
            "portable summary",
            100,
        );
        for message in [request, summary] {
            storage
                .add_message(&session.public_id, message)
                .await
                .unwrap();
        }
        let effective =
            filter_to_effective_history(storage.get_history(&session.public_id).await.unwrap());
        assert_eq!(effective.len(), 2);
        assert!(
            !serde_json::to_string(&effective)
                .unwrap()
                .contains("new-checkpoint")
        );
    }

    #[tokio::test]
    async fn codex_native_compaction_rejects_missing_duplicate_incomplete_or_foreign_state() {
        let valid = codex_compaction_output("checkpoint-state");
        let mut missing = valid.clone();
        missing.items.pop();
        let mut duplicate = valid.clone();
        duplicate.items.push(valid.items.last().unwrap().clone());
        let mut blank = valid.clone();
        if let ChatOutputItem::Opaque(item) = blank.items.last_mut().unwrap() {
            item.payload["encrypted_content"] = serde_json::json!("");
        }
        let mut incomplete = valid.clone();
        incomplete.status = Some(ChatOutputStatus::Incomplete);
        let mut foreign = valid.clone();
        foreign.provenance.as_mut().unwrap().provider = "openai".into();
        for output in [missing, duplicate, blank, incomplete, foreign] {
            let provider = ScriptedProvider::new(false, vec![Attempt::output(output)]);
            assert!(
                SessionCompaction::new()
                    .process_codex(
                        &[],
                        provider,
                        codex::CompactionContext {
                            target: &codex_target(),
                            tools: &[],
                            threshold: None
                        },
                        &retry_config(0),
                        None,
                        &test_ledger().await,
                    )
                    .await
                    .is_err()
            );
        }
    }

    async fn process_script(
        provider: Arc<ScriptedProvider>,
        max_retries: usize,
    ) -> Result<CompactionResult> {
        SessionCompaction::new()
            .process(
                &[],
                provider,
                "model",
                &retry_config(max_retries),
                None,
                &test_ledger().await,
            )
            .await
    }

    #[tokio::test]
    async fn structured_compaction_collects_deltas_and_final_snapshots_once() {
        for with_deltas in [false, true] {
            let chunks =
                structured_summary("Keep the implementation and continue testing.", with_deltas);
            let provider = ScriptedProvider::new(
                true,
                vec![Attempt::Stream(chunks.into_iter().map(Ok).collect())],
            );
            let result = process_script(provider.clone(), 0).await.unwrap();
            assert_eq!(
                result.summary,
                "Keep the implementation and continue testing."
            );
            assert_eq!(provider.calls.load(Ordering::Relaxed), 1);
        }
    }

    #[tokio::test]
    async fn legacy_compaction_collects_text_without_thinking() {
        let provider = ScriptedProvider::new(
            true,
            vec![Attempt::Stream(vec![
                Ok(StreamChunk::Thinking("not a summary".into())),
                Ok(StreamChunk::Text("Keep the implementation".into())),
                Ok(StreamChunk::Text(" and continue testing.".into())),
                Ok(StreamChunk::Done {
                    finish_reason: FinishReason::Stop,
                }),
            ])],
        );
        assert_eq!(
            process_script(provider, 0).await.unwrap().summary,
            "Keep the implementation and continue testing."
        );
    }

    #[tokio::test]
    async fn blank_compaction_retries_are_bounded_for_both_formats() {
        for streaming in [false, true] {
            for text in ["", " \n\t "] {
                let attempts = (0..3)
                    .map(|_| {
                        if streaming {
                            Attempt::Stream(
                                structured_summary(text, true).into_iter().map(Ok).collect(),
                            )
                        } else {
                            Attempt::output(ChatOutput::from_projections(
                                None,
                                Some(text.into()),
                                None,
                                None,
                                Some(FinishReason::Stop),
                            ))
                        }
                    })
                    .collect();
                let provider = ScriptedProvider::new(streaming, attempts);
                let error = process_script(provider.clone(), 2).await.unwrap_err();
                assert!(error.to_string().contains("empty summary"));
                assert_eq!(provider.calls.load(Ordering::Relaxed), 3);
            }
        }
    }

    #[tokio::test]
    async fn blank_compaction_can_retry_to_a_valid_summary() {
        let provider = Arc::new(MockCompactionProvider::new(vec![
            Ok(" \n ".into()),
            Ok("Valid continuation summary".into()),
        ]));
        let result = SessionCompaction::new()
            .process(
                &[],
                provider.clone(),
                "model",
                &retry_config(1),
                None,
                &test_ledger().await,
            )
            .await
            .unwrap();
        assert_eq!(result.summary, "Valid continuation summary");
        assert_eq!(provider.call_count(), 2);
    }

    #[tokio::test]
    async fn incomplete_failed_and_unterminated_streams_cannot_replace_history() {
        for status in [ChatOutputStatus::Incomplete, ChatOutputStatus::Failed] {
            let mut chunks = structured_summary("partial summary", false);
            *chunks.last_mut().unwrap() =
                StreamChunk::Structured(StructuredStreamEvent::ResponseTerminal {
                    status,
                    usage: None,
                    finish_reason: None,
                    detail: Some("not complete".into()),
                });
            let provider = ScriptedProvider::new(
                true,
                vec![Attempt::Stream(chunks.into_iter().map(Ok).collect())],
            );
            assert!(process_script(provider, 0).await.is_err());
        }
        let mut chunks = structured_summary("partial summary", true);
        chunks.pop();
        let metadata = chunks.first_mut().unwrap();
        if let StreamChunk::Structured(StructuredStreamEvent::ResponseMetadata { status, .. }) =
            metadata
        {
            *status = Some(ChatOutputStatus::Completed);
        }
        for chunks in [
            chunks,
            vec![StreamChunk::Text("partial legacy summary".into())],
        ] {
            let provider = ScriptedProvider::new(
                true,
                vec![Attempt::Stream(chunks.into_iter().map(Ok).collect())],
            );
            let error = process_script(provider, 0).await.unwrap_err();
            assert!(error.to_string().contains("without a terminal event"));
        }
    }

    #[tokio::test]
    async fn failed_stream_retry_discards_previous_partial_text() {
        let provider = ScriptedProvider::new(
            true,
            vec![
                Attempt::Stream(vec![
                    Ok(StreamChunk::Text("discard me".into())),
                    Err(LLMError::GenericError("transport failed".into())),
                ]),
                Attempt::Stream(
                    structured_summary("Valid continuation summary", true)
                        .into_iter()
                        .map(Ok)
                        .collect(),
                ),
            ],
        );
        assert_eq!(
            process_script(provider.clone(), 1).await.unwrap().summary,
            "Valid continuation summary"
        );
        assert_eq!(provider.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn unfinished_structured_items_are_rejected_even_with_a_success_terminal() {
        let chunks = structured_summary("unfinished summary", true)
            .into_iter()
            .filter(|chunk| {
                !matches!(
                    chunk,
                    StreamChunk::Structured(StructuredStreamEvent::ItemCompleted { .. })
                )
            })
            .map(Ok)
            .collect();
        let provider = ScriptedProvider::new(true, vec![Attempt::Stream(chunks)]);
        assert!(process_script(provider, 0).await.is_err());
    }

    #[tokio::test]
    async fn compaction_rejects_tool_calls_even_when_text_and_status_look_successful() {
        let output = ChatOutput::from_projections(
            None,
            Some("not a usable continuation".into()),
            Some(vec![querymt::ToolCall {
                id: "call".into(),
                call_type: "function".into(),
                function: querymt::FunctionCall {
                    name: "read".into(),
                    arguments: "{}".into(),
                },
            }]),
            None,
            Some(FinishReason::Stop),
        );
        let provider = ScriptedProvider::new(false, vec![Attempt::output(output)]);
        assert!(
            process_script(provider, 0)
                .await
                .unwrap_err()
                .to_string()
                .contains("did not complete successfully")
        );
    }

    #[tokio::test]
    async fn reasoning_and_refusal_are_not_compaction_summaries() {
        let outputs = [
            ChatOutput::from_projections(
                Some("reasoning only".into()),
                None,
                None,
                None,
                Some(FinishReason::Stop),
            ),
            ChatOutput {
                items: vec![ChatOutputItem::Message(ChatMessageItem {
                    parts: vec![ChatMessagePart::Refusal {
                        refusal: "I cannot summarize".into(),
                        extensions: Default::default(),
                    }],
                    ..match message_item("") {
                        ChatOutputItem::Message(message) => message,
                        _ => unreachable!(),
                    }
                })],
                status: Some(ChatOutputStatus::Completed),
                ..ChatOutput::default()
            },
        ];
        for output in outputs {
            let provider = ScriptedProvider::new(false, vec![Attempt::output(output)]);
            assert!(
                process_script(provider, 0)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("empty summary")
            );
        }
    }

    #[tokio::test]
    async fn unsuccessful_non_streaming_and_legacy_finishes_are_rejected() {
        for finish_reason in [
            FinishReason::Length,
            FinishReason::ContentFilter,
            FinishReason::Error,
            FinishReason::ToolCalls,
        ] {
            let output = ChatOutput::from_projections(
                None,
                Some("not a complete summary".into()),
                None,
                None,
                Some(finish_reason),
            );
            let provider = ScriptedProvider::new(false, vec![Attempt::output(output)]);
            assert!(
                process_script(provider, 0)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("did not complete successfully")
            );
            let provider = ScriptedProvider::new(
                true,
                vec![Attempt::Stream(vec![
                    Ok(StreamChunk::Text("not a complete summary".into())),
                    Ok(StreamChunk::Done { finish_reason }),
                ])],
            );
            assert!(process_script(provider, 0).await.is_err());
        }
        for status in [
            ChatOutputStatus::Incomplete,
            ChatOutputStatus::Failed,
            ChatOutputStatus::InProgress,
        ] {
            let mut output = ChatOutput::from_projections(
                None,
                Some("partial summary".into()),
                None,
                None,
                Some(FinishReason::Stop),
            );
            output.status = Some(status);
            assert!(
                process_script(
                    ScriptedProvider::new(false, vec![Attempt::output(output)]),
                    0
                )
                .await
                .is_err()
            );
        }
    }

    // ========================================================================
    // Test Fixtures
    // ========================================================================

    /// Fixture for building test messages
    struct MessageFixture;

    impl MessageFixture {
        fn text_message(id: &str, session_id: &str, role: ChatRole, text: &str) -> AgentMessage {
            AgentMessage {
                id: id.to_string(),
                session_id: session_id.to_string(),
                role,
                parts: vec![MessagePart::Text {
                    content: text.to_string(),
                }],
                created_at: 0,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            }
        }

        fn assistant_message(id: &str, session_id: &str, text: &str) -> AgentMessage {
            Self::text_message(id, session_id, ChatRole::Assistant, text)
        }

        fn user_message(id: &str, session_id: &str, text: &str) -> AgentMessage {
            Self::text_message(id, session_id, ChatRole::User, text)
        }

        fn tool_result_message(
            id: &str,
            session_id: &str,
            call_id: &str,
            content: &str,
        ) -> AgentMessage {
            AgentMessage {
                id: id.to_string(),
                session_id: session_id.to_string(),
                role: ChatRole::Assistant,
                parts: vec![MessagePart::ToolResult {
                    call_id: call_id.to_string(),
                    content: vec![querymt::chat::ToolResultPart::text(content)],
                    is_error: false,
                    tool_name: Some("test_tool".to_string()),
                    tool_arguments: None,
                    compacted_at: None,
                }],
                created_at: 0,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            }
        }

        fn compaction_request_message(id: &str, session_id: &str) -> AgentMessage {
            AgentMessage {
                id: id.to_string(),
                session_id: session_id.to_string(),
                role: ChatRole::User,
                parts: vec![MessagePart::CompactionRequest {
                    original_token_count: 1000,
                }],
                created_at: 0,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            }
        }

        fn compaction_message(id: &str, session_id: &str, summary: &str) -> AgentMessage {
            AgentMessage {
                id: id.to_string(),
                session_id: session_id.to_string(),
                role: ChatRole::Assistant,
                parts: vec![MessagePart::Compaction {
                    summary: summary.to_string(),
                    original_token_count: 1000,
                }],
                created_at: 0,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            }
        }

        fn reasoning_message(id: &str, session_id: &str, reasoning: &str) -> AgentMessage {
            AgentMessage {
                id: id.to_string(),
                session_id: session_id.to_string(),
                role: ChatRole::Assistant,
                parts: vec![MessagePart::Reasoning {
                    item: querymt::chat::ChatReasoningItem {
                        id: None,
                        summary: Vec::new(),
                        content: vec![querymt::chat::ChatReasoningPart::text(reasoning)],
                        encrypted_content: None,
                        signature: None,
                        status: None,
                        extensions: Default::default(),
                    },
                    time_ms: Some(100),
                }],
                created_at: 0,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            }
        }

        /// Create a simple conversation with user/assistant exchanges
        fn simple_conversation(session_id: &str) -> Vec<AgentMessage> {
            vec![
                Self::user_message("1", session_id, "Hello"),
                Self::assistant_message("2", session_id, "Hi there"),
                Self::user_message("3", session_id, "How are you?"),
                Self::assistant_message("4", session_id, "I'm doing great!"),
            ]
        }

        /// Create a conversation with tool usage
        fn conversation_with_tools(session_id: &str) -> Vec<AgentMessage> {
            vec![
                Self::user_message("1", session_id, "List the files"),
                Self::tool_result_message("2", session_id, "call1", "file1.txt\nfile2.txt"),
                Self::user_message("3", session_id, "Read file1.txt"),
                Self::tool_result_message("4", session_id, "call2", "File contents here"),
            ]
        }

        /// Create a long conversation (for token counting tests)
        fn long_conversation(session_id: &str) -> Vec<AgentMessage> {
            let long_text = "a".repeat(1000);
            vec![
                Self::user_message("1", session_id, &long_text),
                Self::tool_result_message("2", session_id, "call1", &long_text),
                Self::user_message("3", session_id, &long_text),
                Self::tool_result_message("4", session_id, "call2", &long_text),
            ]
        }
    }

    /// Fixture for SessionCompaction service
    struct CompactionFixture {
        service: SessionCompaction,
    }

    impl CompactionFixture {
        fn new() -> Self {
            Self {
                service: SessionCompaction::new(),
            }
        }

        async fn process_with_summary(
            &self,
            messages: &[AgentMessage],
            summary_response: &str,
        ) -> Result<CompactionResult> {
            let mock = MockCompactionProvider::with_summary(summary_response);
            self.service
                .process(
                    messages,
                    Arc::new(mock),
                    "test-model",
                    &RetryConfig::default(),
                    None,
                    &test_ledger().await,
                )
                .await
        }
    }

    // ========================================================================
    // Helper Functions (backwards compatibility)
    // ========================================================================

    fn make_text_message(id: &str, session_id: &str, role: ChatRole, text: &str) -> AgentMessage {
        MessageFixture::text_message(id, session_id, role, text)
    }

    fn make_compaction_request_message(id: &str, session_id: &str) -> AgentMessage {
        MessageFixture::compaction_request_message(id, session_id)
    }

    fn make_compaction_message(id: &str, session_id: &str, summary: &str) -> AgentMessage {
        MessageFixture::compaction_message(id, session_id, summary)
    }

    #[test]
    fn test_filter_returns_all_if_no_compaction() {
        let messages = vec![
            make_text_message("1", "s1", ChatRole::User, "Hello"),
            make_text_message("2", "s1", ChatRole::Assistant, "Hi there"),
            make_text_message("3", "s1", ChatRole::User, "How are you?"),
        ];

        let filtered = filter_to_effective_history(messages.clone());
        assert_eq!(filtered.len(), 3);
        assert_eq!(filtered[0].id, "1");
    }

    #[test]
    fn test_filter_returns_after_compaction() {
        let messages = vec![
            make_text_message("1", "s1", ChatRole::User, "Old message"),
            make_text_message("2", "s1", ChatRole::Assistant, "Old response"),
            make_compaction_request_message("3", "s1"),
            make_compaction_message("4", "s1", "Summary of previous conversation"),
            make_text_message("5", "s1", ChatRole::User, "New message"),
            make_text_message("6", "s1", ChatRole::Assistant, "New response"),
        ];

        let filtered = filter_to_effective_history(messages);
        assert_eq!(filtered.len(), 4);
        assert_eq!(filtered[0].id, "3"); // CompactionRequest (user)
        assert_eq!(filtered[1].id, "4"); // Compaction (assistant)
        assert_eq!(filtered[2].id, "5");
        assert_eq!(filtered[3].id, "6");
    }

    #[test]
    fn test_filter_uses_last_compaction() {
        let messages = vec![
            make_text_message("1", "s1", ChatRole::User, "Very old"),
            make_compaction_request_message("2", "s1"),
            make_compaction_message("3", "s1", "First summary"),
            make_text_message("4", "s1", ChatRole::User, "Medium old"),
            make_compaction_request_message("5", "s1"),
            make_compaction_message("6", "s1", "Second summary"),
            make_text_message("7", "s1", ChatRole::User, "Recent"),
        ];

        let filtered = filter_to_effective_history(messages);
        assert_eq!(filtered.len(), 3);
        assert_eq!(filtered[0].id, "5"); // Second CompactionRequest
        assert_eq!(filtered[1].id, "6"); // Second Compaction
        assert_eq!(filtered[2].id, "7");
    }

    /// Compaction must drop the pre-compaction structured exchange completely:
    /// its canonical output (with continuation state) never leaks back into
    /// effective replay, and kept turns retain complete call/result groups.
    #[test]
    fn test_filter_drops_compacted_structured_exchange_without_dangling_results() {
        use querymt::chat::{
            ChatFunctionCallItem, ChatMessageItem, ChatMessagePart, ChatOutput, ChatOutputItem,
        };

        fn structured_assistant(id: &str, session_id: &str, call_id: &str) -> AgentMessage {
            AgentMessage {
                id: id.to_string(),
                session_id: session_id.to_string(),
                role: ChatRole::Assistant,
                parts: vec![MessagePart::Output {
                    output: ChatOutput {
                        items: vec![
                            ChatOutputItem::Message(ChatMessageItem {
                                id: None,
                                role: ChatRole::Assistant,
                                phase: None,
                                status: None,
                                parts: vec![ChatMessagePart::Text {
                                    text: "calling tool".into(),
                                    annotations: Vec::new(),
                                    extensions: Default::default(),
                                }],
                                extensions: Default::default(),
                            }),
                            ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                                item_id: Some(format!("item_{call_id}")),
                                call_id: call_id.to_string(),
                                name: "shell".into(),
                                arguments: "{}".into(),
                                status: None,
                                extensions: Default::default(),
                            }),
                        ],
                        ..ChatOutput::default()
                    },
                }],
                created_at: 0,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            }
        }

        fn tool_result_user(id: &str, session_id: &str, call_id: &str) -> AgentMessage {
            AgentMessage {
                id: id.to_string(),
                session_id: session_id.to_string(),
                role: ChatRole::User,
                parts: vec![MessagePart::ToolResult {
                    call_id: call_id.to_string(),
                    content: vec![querymt::chat::ToolResultPart::text("output")],
                    is_error: false,
                    tool_name: Some("shell".to_string()),
                    tool_arguments: Some("{}".to_string()),
                    compacted_at: None,
                }],
                created_at: 0,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            }
        }

        let messages = vec![
            // Old structured exchange: compacted away, including its calls.
            structured_assistant("a1", "s1", "call_old"),
            tool_result_user("r1", "s1", "call_old"),
            // Compaction pair.
            make_compaction_request_message("c1", "s1"),
            make_compaction_message("c2", "s1", "Summary"),
            // New structured exchange after compaction: kept intact.
            structured_assistant("a2", "s1", "call_new"),
            tool_result_user("r2", "s1", "call_new"),
        ];

        let filtered = filter_to_effective_history(messages);
        assert_eq!(filtered.len(), 4);
        assert_eq!(filtered[0].id, "c1");

        // The old canonical output is absent from effective replay.
        let serialized = serde_json::to_string(&filtered).unwrap();
        assert!(
            !serialized.contains("call_old"),
            "compacted structured exchange must not leak into effective history"
        );

        // Every kept tool result still has its matching call in a kept
        // assistant message: no dangling dependencies.
        let kept_call_ids: std::collections::HashSet<String> = filtered
            .iter()
            .flat_map(|m| m.function_calls())
            .map(|call| call.id.clone())
            .collect();
        for msg in &filtered {
            for part in &msg.parts {
                if let MessagePart::ToolResult { call_id, .. } = part {
                    assert!(
                        kept_call_ids.contains(call_id.as_str()),
                        "tool result {call_id} has no matching call in effective history"
                    );
                }
            }
        }
    }

    /// Structured turns must be sized for compaction estimates like their
    /// legacy projections are, so token thresholds trigger on real content.
    #[test]
    fn test_estimate_counts_structured_output_parts() {
        use querymt::chat::{ChatMessageItem, ChatMessagePart, ChatOutput, ChatOutputItem};

        let structured = AgentMessage {
            id: "a1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Output {
                output: ChatOutput {
                    items: vec![ChatOutputItem::Message(ChatMessageItem {
                        id: None,
                        role: ChatRole::Assistant,
                        phase: None,
                        status: None,
                        parts: vec![ChatMessagePart::Text {
                            text: "x".repeat(400),
                            annotations: Vec::new(),
                            extensions: Default::default(),
                        }],
                        extensions: Default::default(),
                    })],
                    ..ChatOutput::default()
                },
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        let legacy_equivalent = AgentMessage {
            parts: vec![MessagePart::Text {
                content: "x".repeat(400),
            }],
            ..structured.clone()
        };

        let compactor = SessionCompaction::new();
        let structured_estimate = compactor.estimate_messages_tokens(&[structured], None);
        let legacy_estimate = compactor.estimate_messages_tokens(&[legacy_equivalent], None);
        assert_eq!(
            structured_estimate, legacy_estimate,
            "structured output must be sized like its legacy projection"
        );
    }

    #[test]
    fn test_has_compaction() {
        let messages_with = vec![
            make_text_message("1", "s1", ChatRole::User, "Hello"),
            make_compaction_message("2", "s1", "Summary"),
        ];

        let messages_without = vec![
            make_text_message("1", "s1", ChatRole::User, "Hello"),
            make_text_message("2", "s1", ChatRole::Assistant, "Hi"),
        ];

        assert!(has_compaction(&messages_with));
        assert!(!has_compaction(&messages_without));
    }

    #[test]
    fn test_get_last_compaction() {
        let messages = vec![
            make_compaction_message("1", "s1", "First summary"),
            make_text_message("2", "s1", ChatRole::User, "Hello"),
            make_compaction_message("3", "s1", "Second summary"),
        ];

        let last = get_last_compaction(&messages);
        assert!(last.is_some());
        if let Some(MessagePart::Compaction { summary, .. }) = last {
            assert_eq!(summary, "Second summary");
        } else {
            panic!("Expected Compaction part");
        }
    }

    #[test]
    fn test_create_compaction_messages() {
        let (req, sum) =
            SessionCompaction::create_compaction_messages("session1", "Test summary", 5000);

        // Request message (user side)
        assert_eq!(req.session_id, "session1");
        assert_eq!(req.role, ChatRole::User);
        assert_eq!(req.parts.len(), 1);
        assert!(matches!(
            req.parts[0],
            MessagePart::CompactionRequest {
                original_token_count: 5000
            }
        ));

        // Summary message (assistant side)
        assert_eq!(sum.session_id, "session1");
        assert_eq!(sum.role, ChatRole::Assistant);
        assert_eq!(sum.parts.len(), 1);
        assert_eq!(sum.parent_message_id.as_deref(), Some(req.id.as_str()));

        if let MessagePart::Compaction {
            summary,
            original_token_count,
        } = &sum.parts[0]
        {
            assert_eq!(summary, "Test summary");
            assert_eq!(*original_token_count, 5000);
        } else {
            panic!("Expected Compaction part");
        }
    }

    #[test]
    fn test_retry_config_default() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.initial_backoff_ms, 1000);
        assert!((config.backoff_multiplier - 2.0).abs() < f64::EPSILON);
    }

    // ========================================================================
    // New Unit Tests Using Fixtures
    // ========================================================================

    #[test]
    fn test_message_fixture_creates_text_messages() {
        let msg = MessageFixture::user_message("1", "sess1", "test");
        assert_eq!(msg.id, "1");
        assert_eq!(msg.session_id, "sess1");
        assert_eq!(msg.role, ChatRole::User);
        assert_eq!(msg.parts.len(), 1);
    }

    #[test]
    fn test_message_fixture_simple_conversation() {
        let conv = MessageFixture::simple_conversation("s1");
        assert_eq!(conv.len(), 4);
        assert_eq!(conv[0].role, ChatRole::User);
        assert_eq!(conv[1].role, ChatRole::Assistant);
    }

    #[test]
    fn test_estimate_messages_tokens_text_parts() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::simple_conversation("s1");

        let tokens = fixture.service.estimate_messages_tokens(&messages, None);
        assert!(tokens > 0, "Should count tokens for text messages");
    }

    #[test]
    fn test_estimate_messages_tokens_tool_results() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::conversation_with_tools("s1");

        let tokens = fixture.service.estimate_messages_tokens(&messages, None);
        assert!(tokens > 0, "Should count tokens for tool results");
    }

    #[test]
    fn test_estimate_messages_tokens_mixed_parts() {
        let fixture = CompactionFixture::new();
        let messages = vec![
            MessageFixture::user_message("1", "s1", "User request"),
            MessageFixture::reasoning_message("2", "s1", "Let me think about this"),
            MessageFixture::tool_result_message("3", "s1", "c1", "Tool output"),
            MessageFixture::assistant_message("4", "s1", "Final response"),
        ];

        let tokens = fixture.service.estimate_messages_tokens(&messages, None);
        assert!(tokens > 0, "Should count tokens for all part types");
    }

    #[test]
    fn test_estimate_messages_tokens_empty() {
        let fixture = CompactionFixture::new();
        let messages: Vec<AgentMessage> = vec![];

        let tokens = fixture.service.estimate_messages_tokens(&messages, None);
        assert_eq!(tokens, 0, "Empty messages should have 0 tokens");
    }

    #[test]
    fn test_estimate_messages_tokens_long_conversation() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::long_conversation("s1");

        let tokens = fixture.service.estimate_messages_tokens(&messages, None);
        assert!(
            tokens > 500,
            "Long conversation should have significant token count"
        );
    }

    #[test]
    fn test_build_compaction_messages_appends_prompt() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::simple_conversation("s1");

        let chat_messages = fixture
            .service
            .build_compaction_messages(&messages, None, None)
            .unwrap();

        // Should have all original messages + compaction prompt
        assert_eq!(
            chat_messages.len(),
            messages.len() + 1,
            "Should append compaction prompt"
        );

        // Last message should be the compaction prompt
        assert_eq!(chat_messages.last().unwrap().role, ChatRole::User);
        assert!(
            chat_messages
                .last()
                .unwrap()
                .text()
                .contains("conversation")
        );
    }

    #[test]
    fn test_build_compaction_messages_preserves_roles() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::simple_conversation("s1");

        let chat_messages = fixture
            .service
            .build_compaction_messages(&messages, None, None)
            .unwrap();

        // Check that roles are preserved (excluding the appended prompt)
        assert_eq!(chat_messages[0].role, ChatRole::User);
        assert_eq!(chat_messages[1].role, ChatRole::Assistant);
        assert_eq!(chat_messages[2].role, ChatRole::User);
        assert_eq!(chat_messages[3].role, ChatRole::Assistant);
    }

    #[test]
    fn test_build_compaction_messages_preserves_pdf_content() {
        use base64::Engine as _;

        let fixture = CompactionFixture::new();
        let message = AgentMessage {
            id: "pdf-message".into(),
            session_id: "s1".into(),
            role: ChatRole::User,
            parts: vec![MessagePart::Prompt {
                blocks: vec![crate::acp::protocol::ContentBlock::Resource(
                    crate::acp::protocol::EmbeddedResource::new(
                        crate::acp::protocol::EmbeddedResourceResource::BlobResourceContents(
                            crate::acp::protocol::BlobResourceContents::new(
                                base64::engine::general_purpose::STANDARD.encode(b"%PDF"),
                                "attachment:///document.pdf",
                            )
                            .mime_type("application/pdf".to_string()),
                        ),
                    ),
                )],
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };

        let chat_messages = fixture
            .service
            .build_compaction_messages(&[message], None, None)
            .unwrap();
        assert!(matches!(
            &chat_messages[0].portable_input_parts()[0],
            querymt::chat::ChatInputPart::Attachment(media)
                if media.kind == querymt::chat::MediaKind::Document
        ));
    }

    #[tokio::test]
    async fn test_process_invalid_attachment_does_not_call_provider() {
        let service = SessionCompaction::new();
        let message = AgentMessage {
            id: "invalid-image".into(),
            session_id: "s1".into(),
            role: ChatRole::User,
            parts: vec![MessagePart::Prompt {
                blocks: vec![crate::acp::protocol::ContentBlock::Image(
                    crate::acp::protocol::ImageContent::new("%%%", "image/png"),
                )],
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        let provider = Arc::new(MockCompactionProvider::with_summary("must not be called"));

        let result = service
            .process(
                &[message],
                provider.clone(),
                "model",
                &RetryConfig::default(),
                None,
                &test_ledger().await,
            )
            .await;

        assert!(result.is_err());
        assert_eq!(provider.call_count(), 0);
    }

    #[tokio::test]
    async fn test_process_success_simple() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::simple_conversation("s1");

        let result = fixture
            .process_with_summary(&messages, "This is a summary")
            .await
            .expect("process should succeed");

        assert_eq!(result.summary, "This is a summary");
        assert!(result.original_token_count > 0);
        assert!(result.summary_token_count > 0);
    }

    #[tokio::test]
    async fn test_process_success_with_tools() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::conversation_with_tools("s1");

        let result = fixture
            .process_with_summary(&messages, "Tool execution summary")
            .await
            .expect("process should succeed");

        assert!(!result.summary.is_empty());
        assert!(result.original_token_count > 0);
        assert!(result.summary_token_count > 0);
    }

    #[tokio::test]
    async fn test_process_returns_correct_token_counts() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::long_conversation("s1");

        let original_tokens = fixture.service.estimate_messages_tokens(&messages, None);
        let result = fixture
            .process_with_summary(&messages, "short")
            .await
            .expect("process should succeed");

        assert_eq!(result.original_token_count, original_tokens);
        // Summary "short" should have fewer tokens than original
        assert!(result.summary_token_count <= original_tokens);
    }

    #[tokio::test]
    async fn test_process_with_empty_messages() {
        let fixture = CompactionFixture::new();
        let messages: Vec<AgentMessage> = vec![];

        let result = fixture
            .process_with_summary(&messages, "empty summary")
            .await
            .expect("process should succeed with empty messages");

        assert_eq!(result.original_token_count, 0);
        assert!(!result.summary.is_empty());
    }

    #[tokio::test]
    async fn test_process_retries_on_llm_error() {
        let service = SessionCompaction::new();
        let messages = MessageFixture::simple_conversation("s1");

        // Create a mock that fails twice then succeeds
        let mock = MockCompactionProvider::new(vec![
            Err(LLMError::GenericError("timeout".to_string())),
            Err(LLMError::GenericError("timeout".to_string())),
            Ok("recovered summary".to_string()),
        ]);
        let mock_arc = Arc::new(mock);

        let result = service
            .process(
                &messages,
                mock_arc.clone(),
                "model",
                &RetryConfig::default(),
                None,
                &test_ledger().await,
            )
            .await
            .expect("should succeed after retries");

        assert_eq!(result.summary, "recovered summary");
        assert_eq!(mock_arc.call_count(), 3, "Should have retried twice");
    }

    #[tokio::test]
    async fn test_process_fails_after_max_retries() {
        let service = SessionCompaction::new();
        let messages = MessageFixture::simple_conversation("s1");

        // Create a mock that always fails
        let mock = MockCompactionProvider::new(vec![
            Err(LLMError::GenericError("error1".to_string())),
            Err(LLMError::GenericError("error2".to_string())),
            Err(LLMError::GenericError("error3".to_string())),
            Err(LLMError::GenericError("error4".to_string())),
        ]);
        let mock_arc = Arc::new(mock);

        let result = service
            .process(
                &messages,
                mock_arc.clone(),
                "model",
                &RetryConfig::default(),
                None,
                &test_ledger().await,
            )
            .await;

        assert!(result.is_err(), "should fail after max retries");
        // Default retry config has max_retries=3, so 4 calls (initial + 3 retries)
        assert_eq!(mock_arc.call_count(), 4);
    }

    #[tokio::test]
    async fn test_process_with_custom_retry_config() {
        let service = SessionCompaction::new();
        let messages = MessageFixture::simple_conversation("s1");

        let custom_config = RetryConfig {
            max_retries: 1,
            initial_backoff_ms: 10,
            backoff_multiplier: 1.0,
        };

        let mock = MockCompactionProvider::new(vec![
            Err(LLMError::GenericError("error1".to_string())),
            Err(LLMError::GenericError("error2".to_string())),
        ]);
        let mock_arc = Arc::new(mock);

        let result = service
            .process(
                &messages,
                mock_arc.clone(),
                "model",
                &custom_config,
                None,
                &test_ledger().await,
            )
            .await;

        assert!(result.is_err());
        // With max_retries=1, should have 2 calls (initial + 1 retry)
        assert_eq!(mock_arc.call_count(), 2);
    }

    #[test]
    fn test_create_compaction_messages_structure() {
        let (req, sum) =
            SessionCompaction::create_compaction_messages("session1", "Test summary", 5000);

        assert_eq!(req.session_id, "session1");
        assert_eq!(req.role, ChatRole::User);
        assert_eq!(req.parts.len(), 1);
        assert!(!req.id.is_empty());
        assert!(req.created_at > 0);

        assert_eq!(sum.session_id, "session1");
        assert_eq!(sum.role, ChatRole::Assistant);
        assert_eq!(sum.parts.len(), 1);
        assert!(!sum.id.is_empty());
        assert!(sum.created_at > 0);
        assert_eq!(sum.parent_message_id.as_deref(), Some(req.id.as_str()));
    }

    #[test]
    fn test_create_compaction_messages_with_different_summaries() {
        let (_, sum1) = SessionCompaction::create_compaction_messages("s1", "Summary 1", 1000);
        let (_, sum2) = SessionCompaction::create_compaction_messages("s1", "Summary 2", 2000);

        assert_ne!(
            sum1.id, sum2.id,
            "Different messages should have different IDs"
        );

        if let MessagePart::Compaction {
            summary: s1,
            original_token_count: t1,
        } = &sum1.parts[0]
            && let MessagePart::Compaction {
                summary: s2,
                original_token_count: t2,
            } = &sum2.parts[0]
        {
            assert_ne!(s1, s2);
            assert_ne!(t1, t2);
        }
    }

    #[tokio::test]
    async fn test_process_message_order_preserved() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::simple_conversation("s1");

        let chat_messages = fixture
            .service
            .build_compaction_messages(&messages, None, None)
            .unwrap();

        // Verify first message is the user message from conversation
        if let MessagePart::Text { content } = &messages[0].parts[0] {
            assert!(chat_messages[0].text().contains(content));
        }
    }

    #[test]
    fn test_compaction_message_to_chat_has_no_trailing_whitespace() {
        let (_, sum) = SessionCompaction::create_compaction_messages("s1", "Test summary", 5000);
        let chat = sum.to_chat_message().unwrap();

        let text = chat.text();
        assert!(
            !text.ends_with(char::is_whitespace),
            "Compaction chat message must not end with whitespace, got: {:?}",
            &text[text.len().saturating_sub(30)..]
        );
    }

    #[test]
    fn test_filter_to_effective_history_only_returns_post_compaction() {
        // Simulate a second compaction scenario: old messages, compaction pair, new messages
        let messages = vec![
            MessageFixture::user_message("1", "s1", "Very old message"),
            MessageFixture::assistant_message("2", "s1", "Very old response"),
            MessageFixture::user_message("3", "s1", "Another old message"),
            MessageFixture::assistant_message("4", "s1", "Another old response"),
            MessageFixture::compaction_request_message("5", "s1"),
            MessageFixture::compaction_message("6", "s1", "Summary of old messages"),
            MessageFixture::user_message("7", "s1", "New message after compaction"),
            MessageFixture::assistant_message("8", "s1", "New response"),
            MessageFixture::user_message("9", "s1", "Latest message"),
        ];

        let filtered = filter_to_effective_history(messages);

        // Should contain compaction pair + everything after
        assert_eq!(filtered.len(), 5);
        assert_eq!(filtered[0].id, "5"); // CompactionRequest (user)
        assert_eq!(filtered[1].id, "6"); // Compaction (assistant)
        assert_eq!(filtered[2].id, "7");
        assert_eq!(filtered[3].id, "8");
        assert_eq!(filtered[4].id, "9");

        // Verify the pair starts with user→assistant
        assert_eq!(filtered[0].role, ChatRole::User);
        assert!(
            filtered[0]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::CompactionRequest { .. }))
        );
        assert_eq!(filtered[1].role, ChatRole::Assistant);
        assert!(
            filtered[1]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::Compaction { .. }))
        );
    }

    // ========================================================================
    // is_mesh_timeout_error tests
    // ========================================================================

    #[test]
    fn test_mesh_timeout_network_timeout() {
        let err = LLMError::Transport {
            kind: querymt::error::TransportErrorKind::Timeout,
            message: "network timeout".to_string(),
        };
        assert!(
            is_mesh_timeout_error(&err),
            "network timeout should be detected as mesh timeout"
        );
    }

    #[test]
    fn test_mesh_timeout_eof_error() {
        let err = LLMError::Transport {
            kind: querymt::error::TransportErrorKind::Timeout,
            message: "Eof { name: \"enum\", expect: Small(1) }".to_string(),
        };
        assert!(
            is_mesh_timeout_error(&err),
            "Eof decode error should be detected as mesh timeout"
        );
    }

    #[test]
    fn test_mesh_timeout_dial_failure_is_not_timeout() {
        let err = LLMError::Transport {
            kind: querymt::error::TransportErrorKind::ConnectionRefused,
            message: "dial failure".to_string(),
        };
        assert!(
            !is_mesh_timeout_error(&err),
            "dial failure should NOT be classified as mesh timeout"
        );
    }

    #[test]
    fn test_mesh_timeout_generic_error_is_not_timeout() {
        let err = LLMError::GenericError("some other error".to_string());
        assert!(
            !is_mesh_timeout_error(&err),
            "generic errors should NOT be classified as mesh timeout"
        );
    }

    #[test]
    fn test_mesh_timeout_connection_closed_is_not_timeout() {
        let err = LLMError::Transport {
            kind: querymt::error::TransportErrorKind::ConnectionClosed,
            message: "connection closed".to_string(),
        };
        assert!(
            !is_mesh_timeout_error(&err),
            "connection closed should NOT be classified as mesh timeout"
        );
    }

    #[tokio::test]
    async fn test_process_aborts_on_mesh_timeout() {
        let service = SessionCompaction::new();
        let messages = MessageFixture::simple_conversation("s1");

        // Simulate a mesh timeout on the first attempt — should NOT retry.
        let mock = MockCompactionProvider::new(vec![
            Err(LLMError::Transport {
                kind: querymt::error::TransportErrorKind::Timeout,
                message: "network timeout".to_string(),
            }),
            // This second response should never be reached.
            Ok("should not be used".to_string()),
        ]);
        let mock_arc = Arc::new(mock);

        let result = service
            .process(
                &messages,
                mock_arc.clone(),
                "model",
                &RetryConfig::default(),
                None,
                &test_ledger().await,
            )
            .await;

        assert!(result.is_err(), "should fail on mesh timeout");
        assert_eq!(
            mock_arc.call_count(),
            1,
            "should NOT retry after mesh timeout — only 1 call expected"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("mesh timeout"),
            "error should mention mesh timeout, got: {}",
            err_msg
        );
    }

    // ========================================================================
    // merge_consecutive_tool_result_messages tests
    // ========================================================================

    /// Helper: a User message with a single tool_result part.
    fn make_user_tool_result(id: &str, session_id: &str, call_id: &str) -> AgentMessage {
        AgentMessage {
            id: id.to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::ToolResult {
                call_id: call_id.to_string(),
                content: vec![querymt::chat::ToolResultPart::text("output")],
                is_error: false,
                tool_name: Some("tool".to_string()),
                tool_arguments: None,
                compacted_at: None,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        }
    }

    /// Helper: an Assistant message with tool_use parts.
    fn make_assistant_tool_use(id: &str, session_id: &str) -> AgentMessage {
        AgentMessage {
            id: id.to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::ToolUse(querymt::ToolCall {
                id: "tu-1".to_string(),
                call_type: "function".to_string(),
                function: querymt::FunctionCall {
                    name: "tool".to_string(),
                    arguments: "{}".to_string(),
                },
            })],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        }
    }

    #[test]
    fn test_merge_consecutive_tool_result_messages_combines_split_results() {
        // Simulates legacy DB layout: assistant with 3 tool_use, followed by
        // 3 separate User messages each with 1 tool_result.
        let messages = vec![
            MessageFixture::user_message("1", "s1", "do stuff"),
            make_assistant_tool_use("2", "s1"),
            make_user_tool_result("3", "s1", "call-a"),
            make_user_tool_result("4", "s1", "call-b"),
            make_user_tool_result("5", "s1", "call-c"),
            MessageFixture::assistant_message("6", "s1", "done"),
        ];

        let merged = merge_consecutive_tool_result_messages(messages);

        assert_eq!(merged.len(), 4, "3 tool_result msgs should merge into 1");
        // The merged message should keep the first message's id.
        assert_eq!(merged[2].id, "3");
        assert_eq!(merged[2].role, ChatRole::User);
        // All 3 tool_result parts should be in the merged message.
        let tr_count = merged[2]
            .parts
            .iter()
            .filter(|p| matches!(p, MessagePart::ToolResult { .. }))
            .count();
        assert_eq!(tr_count, 3, "merged message should have 3 ToolResult parts");
    }

    #[test]
    fn test_merge_consecutive_leaves_single_results_alone() {
        // Single tool_result User messages should not be affected.
        let messages = vec![
            MessageFixture::user_message("1", "s1", "hi"),
            make_assistant_tool_use("2", "s1"),
            make_user_tool_result("3", "s1", "call-a"),
            MessageFixture::assistant_message("4", "s1", "ok"),
        ];

        let merged = merge_consecutive_tool_result_messages(messages.clone());
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[2].parts.len(), 1);
    }

    #[test]
    fn test_merge_consecutive_does_not_merge_non_tool_result_user_messages() {
        // Two consecutive User messages where the second is plain text (not a
        // tool result) should not be merged.
        let messages = vec![
            make_assistant_tool_use("1", "s1"),
            make_user_tool_result("2", "s1", "call-a"),
            MessageFixture::user_message("3", "s1", "follow up"),
            MessageFixture::assistant_message("4", "s1", "ok"),
        ];

        let merged = merge_consecutive_tool_result_messages(messages);
        // No merging should happen — 2 and 3 stay separate.
        assert_eq!(merged.len(), 4);
    }

    #[test]
    fn test_filter_to_effective_history_merges_split_tool_results() {
        // End-to-end: filter_to_effective_history should also merge split
        // tool_result messages (safety net for existing DB data).
        let messages = vec![
            MessageFixture::user_message("1", "s1", "do stuff"),
            make_assistant_tool_use("2", "s1"),
            make_user_tool_result("3", "s1", "call-a"),
            make_user_tool_result("4", "s1", "call-b"),
            MessageFixture::assistant_message("5", "s1", "done"),
        ];

        let filtered = filter_to_effective_history(messages);
        assert_eq!(
            filtered.len(),
            4,
            "2 split tool_result msgs should be merged into 1"
        );
    }

    mod accounting_tests {
        use super::*;
        use crate::accounting::{AccountingReason, UsageDisposition};
        use querymt::providers::{BillingClass, ModelPricing};

        fn pricing() -> ModelPricing {
            ModelPricing {
                input: Some(3.0),
                output: Some(15.0),
                cache_read: Some(0.3),
                cache_write: Some(3.75),
                ..Default::default()
            }
        }

        fn usage() -> querymt::Usage {
            querymt::Usage {
                input_tokens: 100,
                output_tokens: 10,
                reasoning_tokens: 5,
                cache_read: 50,
                cache_write: 20,
                cache_write_tiers: None,
            }
        }

        fn cost() -> f64 {
            (100.0 * 3.0 + 15.0 * 15.0 + 50.0 * 0.3 + 20.0 * 3.75) / 1e6
        }

        fn response(text: &str) -> ChatOutput {
            let mut out = ChatOutput::from_projections(
                None,
                Some(text.into()),
                None,
                None,
                Some(FinishReason::Stop),
            );
            out.usage = Some(usage());
            out
        }

        fn legacy_stream(text: &str) -> Vec<std::result::Result<StreamChunk, LLMError>> {
            vec![
                Ok(StreamChunk::Text(text.into())),
                Ok(StreamChunk::Usage(usage())),
                Ok(StreamChunk::Done {
                    finish_reason: FinishReason::Stop,
                }),
            ]
        }

        async fn run(
            provider: Arc<ScriptedProvider>,
            retries: usize,
        ) -> (
            Result<CompactionResult>,
            Arc<crate::accounting::SessionAccounting>,
        ) {
            let (ledger, accounting) = ledger_with(BillingClass::Metered, Some(pricing())).await;
            let history = [MessageFixture::user_message(
                "m1",
                "compaction-session",
                "hello",
            )];
            let result = SessionCompaction::new()
                .process(
                    &history,
                    provider,
                    "model",
                    &retry_config(retries),
                    None,
                    &ledger,
                )
                .await;
            (result, accounting)
        }

        #[tokio::test]
        async fn non_streaming_rejected_summary_retries_and_both_attempts_are_charged() {
            // First summary is empty (rejected by validation), second is usable.
            let provider = ScriptedProvider::new(
                false,
                vec![
                    Attempt::output(response("")),
                    Attempt::output(response("a real summary")),
                ],
            );
            let (result, accounting) = run(provider.clone(), 1).await;
            assert_eq!(result.unwrap().summary, "a real summary");
            assert_eq!(provider.calls.load(Ordering::Relaxed), 2);

            let t = accounting.read(|r| r.session().clone());
            assert_eq!(
                t.priced_attempts, 2,
                "the rejected summary still incurred cost"
            );
            assert_eq!(t.unresolved_attempts, 0);
            // Cache read/write and reasoning are priced, once per attempt.
            assert!((t.known_subtotal_usd - 2.0 * cost()).abs() < 1e-12);
            assert!((t.complete_total_usd().unwrap() - 2.0 * cost()).abs() < 1e-12);
            let compaction =
                accounting.read(|r| r.purpose(crate::accounting::AttemptPurpose::Compaction));
            assert_eq!(compaction.priced_attempts, 2);
        }

        #[tokio::test]
        async fn streaming_rejected_summary_retries_and_both_attempts_are_charged() {
            let provider = ScriptedProvider::new(
                true,
                vec![
                    Attempt::Stream(legacy_stream("")),
                    Attempt::Stream(legacy_stream("a real summary")),
                ],
            );
            let (result, accounting) = run(provider.clone(), 1).await;
            assert_eq!(result.unwrap().summary, "a real summary");
            let t = accounting.read(|r| r.session().clone());
            assert_eq!(t.priced_attempts, 2);
            assert!((t.complete_total_usd().unwrap() - 2.0 * cost()).abs() < 1e-12);
        }

        #[tokio::test]
        async fn exhausted_retries_keep_every_available_charge() {
            let provider = ScriptedProvider::new(
                false,
                vec![
                    Attempt::output(response("")),
                    Attempt::output(response("  ")),
                ],
            );
            let (result, accounting) = run(provider, 1).await;
            assert!(result.is_err());
            let t = accounting.read(|r| r.session().clone());
            assert_eq!(t.priced_attempts, 2);
            assert!((t.known_subtotal_usd - 2.0 * cost()).abs() < 1e-12);
        }

        #[tokio::test]
        async fn interrupted_stream_keeps_partial_usage_and_stays_uncertain() {
            let provider = ScriptedProvider::new(
                true,
                vec![Attempt::Stream(vec![
                    Ok(StreamChunk::Usage(usage())),
                    Ok(StreamChunk::Text("part".into())),
                    Err(LLMError::ProviderError("stream broke".into())),
                ])],
            );
            let (result, accounting) = run(provider, 0).await;
            assert!(result.is_err());
            let t = accounting.read(|r| r.session().clone());
            assert!((t.known_subtotal_usd - cost()).abs() < 1e-12);
            assert_eq!(t.complete_total_usd(), None);
            assert!(t.reasons().contains(&AccountingReason::PartialUsage));
        }

        #[tokio::test]
        async fn confirmed_rejection_adds_no_charge_and_transport_failure_is_uncertain() {
            let (result, accounting) = run(
                ScriptedProvider::new(
                    true,
                    vec![Attempt::SetupError(LLMError::AuthError("bad key".into()))],
                ),
                0,
            )
            .await;
            assert!(result.is_err());
            let t = accounting.read(|r| r.session().clone());
            assert_eq!((t.priced_attempts, t.unresolved_attempts), (0, 0));
            assert!(t.reasons().is_empty());

            let (result, accounting) = run(
                ScriptedProvider::new(
                    true,
                    vec![Attempt::SetupError(LLMError::HttpError("reset".into()))],
                ),
                0,
            )
            .await;
            assert!(result.is_err());
            let t = accounting.read(|r| r.session().clone());
            assert_eq!(t.priced_attempts, 1);
            assert!(t.reasons().contains(&AccountingReason::UsageUnavailable));
        }

        #[tokio::test]
        async fn ownership_comes_from_the_ledger_not_from_history() {
            // History carries a different session id; the charge still lands
            // on the explicitly supplied cycle.
            let provider = ScriptedProvider::new(false, vec![Attempt::output(response("ok"))]);
            let (ledger, accounting) = ledger_with(BillingClass::Metered, Some(pricing())).await;
            let history = [MessageFixture::user_message(
                "m1",
                "some-other-session",
                "hello",
            )];
            SessionCompaction::new()
                .process(&history, provider, "model", &retry_config(0), None, &ledger)
                .await
                .unwrap();
            assert_eq!(accounting.read(|r| r.cycle("cycle-1").priced_attempts), 1);
            assert_eq!(accounting.read(|r| r.session().priced_attempts), 1);
        }

        #[tokio::test]
        async fn excluded_billing_records_usage_without_catalog_charges() {
            let provider = ScriptedProvider::new(false, vec![Attempt::output(response("ok"))]);
            let (ledger, accounting) = ledger_with(BillingClass::Excluded, None).await;
            let history = [MessageFixture::user_message(
                "m1",
                "compaction-session",
                "hello",
            )];
            SessionCompaction::new()
                .process(&history, provider, "model", &retry_config(0), None, &ledger)
                .await
                .unwrap();
            let t = accounting.read(|r| r.session().clone());
            assert_eq!((t.excluded_attempts, t.priced_attempts), (1, 0));
            assert_eq!(t.known_subtotal_usd, 0.0);
        }

        #[tokio::test]
        async fn accounting_failure_prevents_the_provider_call() {
            let provider = ScriptedProvider::new(false, vec![Attempt::output(response("ok"))]);
            let (ledger, _) = ledger_with(BillingClass::Metered, Some(pricing())).await;
            // An attempt owned by another session is rejected by the recorder.
            let foreign = crate::accounting::DispatchLedger {
                operation: Arc::new(crate::accounting::AttemptOperation::new(
                    Arc::new(crate::accounting::AttemptRecorder::new(
                        crate::session_event_stream::SessionEventStream::new(
                            "someone-else",
                            {
                                use crate::session::backend::StorageBackend;
                                crate::session::sqlite_storage::SqliteStorage::connect(
                                    ":memory:".into(),
                                )
                                .await
                                .unwrap()
                                .event_journal()
                            },
                            Arc::new(crate::event_fanout::EventFanout::new()),
                        ),
                    )),
                    &crate::accounting::CycleAccounting::with_id(
                        crate::accounting::SessionAccounting::hydrate("compaction-session", {
                            use crate::session::backend::StorageBackend;
                            crate::session::sqlite_storage::SqliteStorage::connect(
                                ":memory:".into(),
                            )
                            .await
                            .unwrap()
                            .event_journal()
                        })
                        .await
                        .unwrap(),
                        "c",
                    ),
                    crate::accounting::AttemptPurpose::Compaction,
                    "p",
                    "model",
                )),
                basis: ledger.basis.clone(),
            };
            let history = [MessageFixture::user_message(
                "m1",
                "compaction-session",
                "hello",
            )];
            let result = SessionCompaction::new()
                .process(
                    &history,
                    provider.clone(),
                    "model",
                    &retry_config(2),
                    None,
                    &foreign,
                )
                .await;
            assert!(result.is_err());
            assert_eq!(
                provider.calls.load(Ordering::Relaxed),
                0,
                "no paid request without a recorded start"
            );
            let _ = UsageDisposition::Final;
        }
    }
}
