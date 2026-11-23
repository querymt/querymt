//! AI-powered conversation compaction
//!
//! This module implements the AI compaction layer of the 3-layer compaction system,
//! which generates summaries when context threshold is reached.

use crate::agent::agent_config::AgentConfig;
use crate::agent::execution::llm_retry::call_chat_with_retry_if;
use crate::agent::session_actor::TurnAuthority;
use crate::agent::utils::genai;
use crate::session::pruning::{SimpleTokenEstimator, TokenEstimator};
use anyhow::Result;
use futures_util::{FutureExt, StreamExt};
use querymt::chat::{
    ChatCall, ChatOutput, ChatOutputStatus, ChatRole, ChatStreamAccumulator,
    ChatStreamAccumulatorError, ChatStreamFinish, FinishReason,
};
use querymt::error::LLMError;
pub use querymt_agent_core::history::{
    filter_to_effective_history, merge_consecutive_tool_result_messages,
};
use querymt_agent_core::model::{AgentMessage, MessagePart};
use querymt_agent_services::accounting::{AccountingError, DispatchLedger, DispatchMode};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Default compaction prompt used to generate conversation summaries
///
/// Written as a handoff brief for the work still to be done, not a narrative of
/// what happened: the most accuracy-preserving compactions select the facts the
/// next stretch of work needs. The user's own request is appended verbatim by
/// [`original_task_section`], so the model is not relied on to copy it.
pub const COMPACTION_PROMPT: &str = r#"Stop work. Write only a concise handoff brief for continuing our conversation; do not call tools.
This brief will replace the conversation history. Write it for the work that remains, not as a narrative of what happened. Record:
1. Objective - one line describing the required result, including user constraints and preferences that must persist.
2. Established facts - exact paths, relevant symbols or commands, observed values, and important errors/output. Separate verified observations from unverified ideas.
3. Applied changes - each changed or created file, what changed, and whether it was verified.
4. Verification - exact checks run, their outcomes, failures, obstacles, and workarounds.
5. Dead ends - failed approaches that should not be repeated.
6. Remaining plan - outstanding subgoals in execution order, the immediate next command or edit, and the condition for completion.

If needed output was cleared or truncated, name the `session_history` call_id or a narrow command to retrieve it. Do not invent observations."#;

/// The compaction prompt, extended with the user's focus instructions when given.
pub(crate) fn compaction_prompt(instructions: Option<&str>) -> String {
    match instructions.map(str::trim).filter(|s| !s.is_empty()) {
        Some(focus) => format!(
            "{COMPACTION_PROMPT}\n\nThe user asked for this summary to follow these additional instructions. Honor them while still covering the sections above:\n<user_focus>\n{focus}\n</user_focus>"
        ),
        None => COMPACTION_PROMPT.to_string(),
    }
}

/// The active task's identifier, deliverable and acceptance criteria, verbatim,
/// for appending to a compaction summary.
///
/// With the run objective off by default, compaction is the one place that can
/// lose these. Returns `None` when there is no active task, or it has neither a
/// deliverable nor acceptance criteria.
pub(crate) fn active_task_section(task: Option<&crate::session::domain::Task>) -> Option<String> {
    let task = task.filter(|task| task.status == crate::session::domain::TaskStatus::Active)?;
    let deliverable = task
        .expected_deliverable
        .as_deref()
        .filter(|s| !s.is_empty());
    let criteria = task
        .acceptance_criteria
        .as_deref()
        .filter(|s| !s.is_empty());
    if deliverable.is_none() && criteria.is_none() {
        return None;
    }
    let mut lines = vec![format!("Task: {}", task.public_id)];
    if let Some(deliverable) = deliverable {
        lines.push(format!("Deliverable: {deliverable}"));
    }
    if let Some(criteria) = criteria {
        lines.push(format!("Acceptance criteria: {criteria}"));
    }
    Some(format!(
        "<active_task note=\"verbatim, still in force after this summary\">\n{}\n</active_task>",
        lines.join("\n")
    ))
}

/// Upper bound on the verbatim user prompts carried across a compaction.
const ORIGINAL_TASK_MAX_BYTES: usize = 16 * 1024;
/// Most recent user prompts carried across a compaction.
const ORIGINAL_TASK_MAX_PROMPTS: usize = 3;

/// The user's recent prompts, verbatim, for appending to a compaction summary.
///
/// Compaction drops everything before it, so without this the user's actual
/// request survives only if the model happens to restate it. Returns `None`
/// when `messages` hold no user prompt with text (for example when compacting
/// an earlier summary only, which already carries the task).
pub(crate) fn original_task_section(messages: &[AgentMessage]) -> Option<String> {
    let mut prompts: Vec<String> = Vec::new();
    let mut remaining = ORIGINAL_TASK_MAX_BYTES;
    for message in messages.iter().rev() {
        if message.role != ChatRole::User {
            continue;
        }
        for part in message.parts.iter().rev() {
            let MessagePart::Prompt { blocks } = part else {
                continue;
            };
            let text = crate::agent::utils::render_prompt_for_display(blocks);
            if text.is_empty() {
                continue;
            }
            if remaining == 0 || prompts.len() >= ORIGINAL_TASK_MAX_PROMPTS {
                break;
            }
            let text = crate::agent::utils::truncate_to_bytes(&text, remaining);
            remaining = remaining.saturating_sub(text.len());
            prompts.push(text);
        }
        if remaining == 0 || prompts.len() >= ORIGINAL_TASK_MAX_PROMPTS {
            break;
        }
    }
    if prompts.is_empty() {
        return None;
    }
    prompts.reverse();
    let body = prompts
        .iter()
        .map(|p| format!("<user_prompt>\n{p}\n</user_prompt>"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "<original_task note=\"verbatim user prompts before this summary, oldest first\">\n{body}\n</original_task>"
    ))
}

/// Result of a compaction operation
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// The generated summary
    pub summary: String,
    /// Original token count before compaction
    pub original_token_count: usize,
    /// Estimated token count of the summary
    pub summary_token_count: usize,
}

/// Where and how a compaction request is sent and recorded. Shared by every
/// attempt of one compaction.
///
/// The model and the initiating session are not repeated here: they are the
/// ledger's, so the span, the retry events and the accounting cannot disagree
/// with each other. That `provider` is the one serving the ledger's model is
/// the caller's to keep true: build both from one
/// `SessionHandle::resolve_compaction_model`.
#[derive(Clone)]
pub struct CompactionDispatch<'a> {
    /// The LLM provider used for summary generation.
    pub provider: Arc<dyn querymt::chat::ChatProvider>,
    /// Provider errors are retried by `[agent.execution.rate_limit]`, rejected
    /// summaries by `[agent.execution.compaction.retry]`.
    pub config: &'a AgentConfig,
    /// Stops the request in flight (including a stream that has started) and
    /// the waits between retries.
    pub cancel: &'a CancellationToken,
    /// Where every physical attempt is recorded. Required: the initiating
    /// session, cycle and model are explicit and never inferred from the
    /// messages. Each retry is a separate attempt, settled before the summary
    /// is validated so a rejected summary keeps its cost.
    pub ledger: &'a DispatchLedger,
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
    /// * `max_prompt_bytes` - Optional cap on the request size
    /// * `instructions` - Optional user-provided focus (for example from
    ///   `/compact focus on the API changes`) added to the compaction prompt
    /// * `dispatch` - Where and how the request is sent and recorded; the
    ///   model and session come from its ledger
    pub async fn process(
        &self,
        messages: &[AgentMessage],
        max_prompt_bytes: Option<usize>,
        instructions: Option<&str>,
        dispatch: &CompactionDispatch<'_>,
    ) -> Result<CompactionResult> {
        self.process_authorized(
            messages,
            max_prompt_bytes,
            instructions,
            dispatch,
            &TurnAuthority::Local,
        )
        .await
    }

    pub(crate) async fn process_authorized(
        &self,
        messages: &[AgentMessage],
        max_prompt_bytes: Option<usize>,
        instructions: Option<&str>,
        dispatch: &CompactionDispatch<'_>,
        authority: &TurnAuthority,
    ) -> Result<CompactionResult> {
        // Estimate original token count
        // The compaction request carries no tool records, so none are counted.
        let original_token_count =
            crate::session::pruning::estimate_history_tokens(messages, max_prompt_bytes, false)
                .map_err(|error| LLMError::GenericError(error.to_string()))?;

        // Build the compaction request
        let chat_messages = self.build_compaction_messages_with_instructions(
            messages,
            max_prompt_bytes,
            instructions,
        )?;

        // Call LLM with retry logic
        let summary = self
            .call_with_retry(&chat_messages, dispatch, authority)
            .await?;

        // The summary is validated before this point; the verbatim task is
        // appended afterwards so it can never rescue an empty summary.
        let summary = match original_task_section(messages) {
            Some(task) => format!("{summary}\n\n{task}"),
            None => summary,
        };
        let summary_token_count = self.estimator.estimate(&summary);

        Ok(CompactionResult {
            summary,
            original_token_count,
            summary_token_count,
        })
    }

    /// Build chat messages for compaction request
    #[cfg_attr(not(test), allow(dead_code))]
    fn build_compaction_messages(
        &self,
        messages: &[AgentMessage],
        max_prompt_bytes: Option<usize>,
    ) -> Result<Vec<querymt::chat::ChatMessage>> {
        self.build_compaction_messages_with_instructions(messages, max_prompt_bytes, None)
    }

    fn build_compaction_messages_with_instructions(
        &self,
        messages: &[AgentMessage],
        max_prompt_bytes: Option<usize>,
        instructions: Option<&str>,
    ) -> Result<Vec<querymt::chat::ChatMessage>> {
        // The summary needs the conversation, not the tool definitions: the
        // request carries no tool list, so tool baselines and tool changes are
        // left out by the projection (`tool_changes: false`), which also keeps
        // native provider output out (no output target).
        let mut chat_messages = querymt_agent_core::projection::project_request(
            messages,
            &querymt_agent_core::projection::RequestTarget {
                output: None,
                max_prompt_bytes,
                tool_changes: false,
            },
        )?
        .messages;

        // Add the compaction prompt as a user message
        chat_messages.push(
            querymt::chat::ChatMessage::user()
                .text(compaction_prompt(instructions))
                .build(),
        );

        Ok(chat_messages)
    }

    /// Send the compaction request and return a valid summary.
    ///
    /// Two loops with different owners. The inner one is the shared model
    /// request retry ([`call_chat_with_retry_if`]): a provider error is retried
    /// only when `LLMError::is_retryable` (and it is not a request timeout),
    /// after the wait its class asks for, at most `rate_limit.max_retries`
    /// requests, and it stops when `cancel` fires. A request in flight is
    /// stopped by `cancel` too, keeping the usage it had reported.
    /// A stream that closes before its terminal event is a lost connection
    /// and is retried like one. The outer one is compaction's own: a reply that is not a usable
    /// summary (empty, refused, cut off) is retried up to
    /// `compaction.retry.max_retries` times, after `initial_backoff_ms`
    /// multiplied by `backoff_multiplier` for each further retry.
    ///
    /// Prefers streaming when the provider supports it.  The streaming path
    /// uses per-chunk timeouts (typically 60 s) instead of a single total
    /// request-response timeout, keeping the connection alive while the model
    /// generates the compaction summary.  This avoids the 10 s libp2p default
    /// timeout that caused compaction failures on remote mesh nodes.
    ///
    /// Request timeouts (the remote side is probably still working) are never
    /// retried, to prevent queuing duplicate inference work on the remote GPU.
    #[tracing::instrument(
        name = "agent.compaction.call_llm",
        skip_all,
        fields(
            otel.name = %format!("chat {}", dispatch.ledger.operation.model()),
            otel.kind = "client",
            gen_ai.operation.name = "chat",
            gen_ai.request.model = dispatch.ledger.operation.model(),
            gen_ai.conversation.id = dispatch.ledger.operation.session_id(),
            session.id = dispatch.ledger.operation.session_id(),
            gen_ai.request.stream = tracing::field::Empty,
            querymt.compaction = true,
        )
    )]
    async fn call_with_retry(
        &self,
        messages: &[querymt::chat::ChatMessage],
        dispatch: &CompactionDispatch<'_>,
        authority: &TurnAuthority,
    ) -> Result<String> {
        let CompactionDispatch {
            provider,
            config,
            cancel,
            ledger,
        } = dispatch;
        // Provider identity is unavailable on ChatProvider; canonical provenance can supply it.
        genai::finish_reason("error");
        let chat_span = tracing::Span::current();
        // What the provider takes cannot change between attempts: it is read
        // once, for the span and for every attempt.
        let streaming = provider.capabilities().streaming;
        chat_span.record("gen_ai.request.stream", streaming);
        let retry = &config.execution_policy.compaction.retry;
        let attempts = genai::RequestAttempts::new(&chat_span);
        // Shared by every request of this compaction: the first chunk is
        // logical across retries.
        let timing = tokio::sync::Mutex::new(genai::StreamTiming::default());
        let mut backoff_ms = retry.initial_backoff_ms;
        let mut last_error = None;

        for summary_attempt in 0..=retry.max_retries {
            if summary_attempt > 0 {
                log::debug!(
                    "Compaction retry attempt {} after {}ms",
                    summary_attempt,
                    backoff_ms
                );
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        return Err(anyhow::anyhow!(
                            "Compaction cancelled while waiting to retry"
                        ));
                    }
                    _ = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {}
                }
                backoff_ms = (backoff_ms as f64 * retry.backoff_multiplier) as u64;
            }

            // Accounting failures and authority denials are fatal and retain
            // their own errors rather than going through provider retry.
            let fatal = std::sync::Mutex::new(None);
            // A request timeout is a failed attempt like any other, but the
            // remote is probably still working: never resend after one.
            let sent = call_chat_with_retry_if(
                config,
                ledger.operation.session_id(),
                &attempts,
                cancel,
                |error| error.is_retryable() && !is_request_timeout(error),
                |reply| matches!(reply, Reply::Rejected(_)),
                || {
                    Self::send_once(
                        messages, dispatch, streaming, &timing, &chat_span, &fatal, authority,
                    )
                },
            )
            .await;
            if let Some(error) = fatal.into_inner().unwrap_or_else(|e| e.into_inner()) {
                return Err(error);
            }

            match sent {
                Ok(Reply::Summary { text, output }) => {
                    genai::output(&output);
                    return Ok(text);
                }
                Ok(Reply::Rejected(error)) => {
                    log::warn!(
                        "Compaction reply rejected as a summary (attempt {})",
                        summary_attempt + 1
                    );
                    last_error = Some(error);
                }
                Err(LLMError::Cancelled) => {
                    return Err(anyhow::anyhow!("Compaction cancelled"));
                }
                Err(error) if is_request_timeout(&error) => {
                    // The remote is still processing. Retrying would just
                    // queue more work on an already-busy remote GPU.
                    genai::llm_error(&error);
                    log::warn!(
                        "Compaction error is a request timeout (remote node likely still \
                         processing). Aborting retries to avoid queuing duplicate work."
                    );
                    return Err(anyhow::anyhow!(
                        "Compaction failed: mesh timeout (remote still processing): {}",
                        error
                    ));
                }
                Err(error) => {
                    // Not retryable, or the provider attempts are used up.
                    genai::llm_error(&error);
                    return Err(anyhow::anyhow!("Compaction request failed: {:?}", error));
                }
            }
        }

        if let Some(error) = &last_error {
            genai::llm_error(error);
        }
        Err(anyhow::anyhow!(
            "Compaction failed after {} retries: {:?}",
            retry.max_retries,
            last_error
        ))
    }

    /// One physical request: open its accounting attempt, send it, settle
    /// it, and say what came back. A reply is `Ok` even when it will be
    /// rejected as a summary, so the shared retry only ever sees provider
    /// errors.
    async fn send_once(
        messages: &[querymt::chat::ChatMessage],
        dispatch: &CompactionDispatch<'_>,
        streaming: bool,
        timing: &tokio::sync::Mutex<genai::StreamTiming>,
        chat_span: &tracing::Span,
        fatal: &std::sync::Mutex<Option<anyhow::Error>>,
        authority: &TurnAuthority,
    ) -> std::result::Result<Reply, LLMError> {
        let CompactionDispatch {
            provider,
            cancel,
            ledger,
            ..
        } = dispatch;
        let park_fatal = |error| {
            *fatal.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
            LLMError::Cancelled
        };
        let park = |error: AccountingError| park_fatal(accounting_failure(error));
        authority
            .check()
            .await
            .map_err(|error| park_fatal(error.into()))?;

        // Prefer streaming to avoid mesh request-response timeout on long
        // inference.  The streaming path uses per-chunk timeouts instead of
        // a single total-request timeout, keeping the connection alive
        // while the model generates.
        // The start is durable before the request is sent. Failing to
        // record is fatal and never retried: no paid call without a ledger.
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
            .map_err(park)?;

        if let Err(error) = authority.check().await {
            attempt_record
                .settle_error(&LLMError::InvalidRequest(error.to_string()), false, None)
                .await
                .map_err(park)?;
            return Err(park_fatal(error.into()));
        }

        let raw = if streaming {
            Self::call_streaming(
                messages,
                provider,
                &mut *timing.lock().await,
                chat_span,
                cancel,
            )
            .await
        } else {
            let call = ChatCall::new(messages);
            match unless_cancelled(cancel, provider.chat(&call)).await {
                Ok(reply) => reply.map_err(|error| StreamFailure {
                    error,
                    response_started: false,
                    usage: None,
                    output: None,
                    sent: true,
                }),
                Err(sent) => Err(StreamFailure::cancelled_before_reply(sent)),
            }
        };

        // Settle before validation: a refused, empty or incomplete summary
        // still incurred whatever usage it reported.
        match raw {
            Ok(output) => {
                let validated = Self::summary_from_output(&output);
                match &validated {
                    Ok(_) => attempt_record.settle_output(&output).await,
                    Err(_) => attempt_record.settle_rejected_output(&output, false).await,
                }
                .map_err(park)?;
                Ok(match validated {
                    Ok(text) => Reply::Summary {
                        text,
                        output: Box::new(output),
                    },
                    Err(error) => Reply::Rejected(error),
                })
            }
            Err(failure) => {
                match &failure.output {
                    Some(output) => attempt_record.settle_rejected_output(output, true).await,
                    // Cancelled before the request was ever polled: nothing
                    // was sent, so nothing can have been billed.
                    None if !failure.sent => attempt_record.settle_cancelled_before_send().await,
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
                .map_err(park)?;
                if failure.output.is_some() {
                    // The stream finished, with a reply that is cut off or
                    // invalid: a rejected summary, not a provider error.
                    Ok(Reply::Rejected(failure.error))
                } else {
                    Err(failure.error)
                }
            }
        }
    }

    /// Collect legacy or structured output, requiring a successful stream completion.
    ///
    /// Failures keep what accounting needs: whether any response bytes were
    /// seen, the usage observed so far, and any partial output.
    async fn call_streaming(
        messages: &[querymt::chat::ChatMessage],
        provider: &Arc<dyn querymt::chat::ChatProvider>,
        timing: &mut genai::StreamTiming,
        chat_span: &tracing::Span,
        cancel: &CancellationToken,
    ) -> std::result::Result<ChatOutput, StreamFailure> {
        timing.start();
        let call = ChatCall::new(messages);
        let mut stream = match unless_cancelled(cancel, provider.chat_stream(&call)).await {
            Ok(opened) => opened.map_err(|error| StreamFailure {
                error,
                response_started: false,
                usage: None,
                output: None,
                sent: true,
            })?,
            Err(sent) => return Err(StreamFailure::cancelled_before_reply(sent)),
        };
        timing.headers_received(chat_span);
        let mut accumulator = ChatStreamAccumulator::new();
        let mut response_started = false;
        let mut terminal_seen = false;
        loop {
            timing.wait_begin();
            // Cancelling stops a stream that is quiet or blocked before its
            // terminal chunk, and keeps the usage it reported so far for the
            // attempt's settlement.
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    timing.wait_end();
                    return Err(StreamFailure::cancelled(
                        response_started,
                        accumulator.output().usage,
                    ));
                }
                next = stream.next() => next,
            };
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
                        sent: true,
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
                    sent: true,
                });
            }
            if querymt::chat::chunk_is_terminal(&chunk) {
                terminal_seen = true;
                break;
            }
        }
        if !terminal_seen {
            return Err(StreamFailure {
                // A lost connection, as the model stream path classifies it:
                // retryable, unlike a typed timeout.
                error: LLMError::Transport {
                    kind: querymt::error::TransportErrorKind::ConnectionClosed,
                    message: "Compaction stream ended without a terminal event".into(),
                },
                response_started,
                usage: accumulator.output().usage,
                output: None,
                sent: true,
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
                    sent: true,
                })
            }
            ChatStreamFinish::Failed { output, error } => {
                let output = merge(output);
                Err(StreamFailure {
                    error: invalid(error),
                    response_started,
                    usage: output.usage.clone(),
                    output: Some(Box::new(output)),
                    sent: true,
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

/// What one physical request returned, after it was settled.
enum Reply {
    /// A usable summary, with the output it came from.
    Summary {
        text: String,
        output: Box<ChatOutput>,
    },
    /// A reply that is not a usable summary (empty, refused, cut off).
    Rejected(LLMError),
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
    /// False only when the provider request was never polled: nothing was
    /// sent, so the attempt cannot have been billed.
    sent: bool,
}

impl StreamFailure {
    /// The compaction was cancelled while the request was in flight.
    fn cancelled(response_started: bool, usage: Option<querymt::Usage>) -> Self {
        Self {
            error: LLMError::Cancelled,
            response_started,
            usage,
            output: None,
            sent: true,
        }
    }

    /// The compaction was cancelled before any reply arrived; `sent` says
    /// whether the provider request had been polled by then.
    fn cancelled_before_reply(sent: bool) -> Self {
        Self {
            sent,
            ..Self::cancelled(false, None)
        }
    }
}

/// Run `request` unless `cancel` fires first (cancellation wins a tie). On
/// cancellation, `Err` says whether `request` had been polled: until it is,
/// a provider request cannot have been sent.
async fn unless_cancelled<T>(
    cancel: &CancellationToken,
    request: impl std::future::Future<Output = T>,
) -> std::result::Result<T, bool> {
    let mut polled = false;
    let outcome = {
        let mut request = std::pin::pin!(request);
        let tracked = std::future::poll_fn(|cx| {
            polled = true;
            request.as_mut().poll(cx)
        });
        tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            reply = tracked => Some(reply),
        }
    };
    outcome.ok_or(polled)
}

fn accounting_failure(error: AccountingError) -> anyhow::Error {
    anyhow::anyhow!("Compaction accounting failure: {error}")
}

/// Check if an LLM error is a request timeout (remote node still processing).
///
/// These errors should **not** be retried because the remote GPU is still busy
/// with the previous request.  Retrying just queues additional inference work,
/// wasting compute and potentially causing cascading timeouts.
///
/// Detection matches on the typed `LLMError::Transport { kind: Timeout, .. }`
/// variant which is produced by `MeshChatProvider::chat_with_tools` when
/// kameo's `RemoteSendError::Timeout` fires, or when a mesh stream's
/// reconnect grace period expires.
fn is_request_timeout(e: &querymt::error::LLMError) -> bool {
    matches!(
        e,
        querymt::error::LLMError::Transport {
            kind: querymt::error::TransportErrorKind::Timeout,
            ..
        }
    )
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
    use crate::test_utils::mocks::MockCompactionProvider;
    use querymt::chat::{
        ChatMessageItem, ChatMessagePart, ChatMessagePartDelta, ChatOutputItem, StreamChunk,
        StructuredStreamEvent,
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
    ) -> (
        DispatchLedger,
        Arc<querymt_agent_services::accounting::SessionAccounting>,
    ) {
        use crate::session::backend::StorageBackend;
        let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap();
        let journal = storage.event_journal();
        let accounting = querymt_agent_services::accounting::SessionAccounting::hydrate(
            "compaction-session",
            journal.clone(),
        )
        .await
        .unwrap();
        let stream = querymt_agent_services::session_event_stream::SessionEventStream::new(
            "compaction-session",
            journal,
            Arc::new(querymt_agent_services::event_fanout::EventFanout::new()),
        );
        let recorder = Arc::new(
            querymt_agent_services::accounting::AttemptRecorder::with_projection(
                stream,
                accounting.clone(),
            ),
        );
        let cycle = querymt_agent_services::accounting::CycleAccounting::with_id(
            accounting.clone(),
            "cycle-1",
        );
        let operation = querymt_agent_services::accounting::AttemptOperation::new(
            recorder,
            &cycle,
            querymt_agent_services::accounting::AttemptPurpose::Compaction,
            "p",
            "model",
        );
        (
            DispatchLedger {
                operation: Arc::new(operation),
                basis: querymt_agent_services::accounting::BillingBasis { billing, pricing },
            },
            accounting,
        )
    }

    #[tokio::test]
    async fn genai_compaction_stream_retry_records_one_canonical_output_without_content() {
        use opentelemetry::{
            Array, Value,
            trace::{SpanKind, Status},
        };
        use querymt::chat::ChatOutputProvenance;
        use querymt_agent_services::tools::trace_support::genai_trace::{
            assert_private, attr, capture,
        };
        use tracing::Instrument;

        // The span's session is the ledger's (the initiating session), never
        // inferred from the history, whatever the history carries.
        for session_id in [None, Some("history-session"), Some("")] {
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
                    Attempt::Stream(vec![Err(LLMError::HttpError("SECRET_ERROR".into()))]),
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
                        None,
                        None,
                        &CompactionDispatch {
                            provider: provider.clone(),
                            config: &*test_config(1).await,
                            cancel: &CancellationToken::new(),
                            ledger: &test_ledger().await,
                        },
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
                Some(&Value::from("compaction-session"))
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
        use opentelemetry::Value;
        use querymt_agent_services::tools::trace_support::genai_trace::{
            assert_private, attr, capture,
        };

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
                // The configuration and ledger (a database each) are built
                // outside the time limit: it is there to catch a wait on the
                // pending tail, not a slow test machine.
                let config = test_config(0).await;
                let ledger = test_ledger().await;
                let (result, spans) = capture(tokio::time::timeout(
                    Duration::from_secs(1),
                    SessionCompaction::new().process(
                        &[],
                        None,
                        None,
                        &CompactionDispatch {
                            provider,
                            config: &config,
                            cancel: &CancellationToken::new(),
                            ledger: &ledger,
                        },
                    ),
                ))
                .await;
                assert_eq!(
                    result
                        .expect("must not await the pending tail")
                        .unwrap()
                        .summary,
                    "SECRET_RESPONSE"
                );
                // The shared retry adds its own helper span; the request is
                // the `chat` span, and there is exactly one.
                let chats: Vec<_> = spans.iter().filter(|s| s.name == "chat model").collect();
                assert_eq!(chats.len(), 1);
                assert_eq!(
                    attr(chats[0], "gen_ai.usage.input_tokens"),
                    Some(&Value::I64(37))
                );
                assert_eq!(
                    attr(chats[0], "gen_ai.usage.output_tokens"),
                    Some(&Value::I64(40))
                );
                assert_private(&spans);
            }
        }
    }

    #[tokio::test]
    async fn genai_compaction_first_chunk_is_logical_across_retry_helper_spans() {
        use opentelemetry::Value;
        use querymt_agent_services::tools::trace_support::genai_trace::{
            assert_private, attr, capture,
        };
        for metadata_first in [false, true] {
            // A retryable error: the shared retry resends after
            // `transport_wait_secs`, which is 0 here.
            let error = || LLMError::HttpError("SECRET_ERROR".into());
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
                None,
                None,
                &CompactionDispatch {
                    provider: provider.clone(),
                    config: &*test_config(1).await,
                    cancel: &CancellationToken::new(),
                    ledger: &test_ledger().await,
                },
            ))
            .await;
            assert!(result.is_ok());
            let chat = spans.iter().find(|span| span.name == "chat model").unwrap();
            let expected = provider.clock.lock().unwrap().1.unwrap();
            assert!(
                matches!(attr(chat, "gen_ai.response.time_to_first_chunk"), Some(Value::F64(actual)) if (actual - expected).abs() < 0.03)
            );
            if !metadata_first {
                // The provider's delay of 20 ms three times (setup of the
                // first request, then setup and first chunk of the retry).
                // The retry itself waits `transport_wait_secs`, 0 here.
                assert!(expected >= 0.06);
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
        use opentelemetry::{Value, trace::Status};
        use querymt_agent_services::tools::trace_support::genai_trace::{
            assert_private, attr, capture,
        };
        let provider = ScriptedProvider::new(
            true,
            vec![Attempt::Stream(vec![Err(LLMError::AuthError(
                "SECRET_ERROR".into(),
            ))])],
        );
        let (result, spans) = capture(process_script(provider, 0)).await;
        assert!(result.is_err());
        // The shared retry adds its own helper span; the request is the
        // `chat` span, and there is exactly one.
        let chats: Vec<_> = spans.iter().filter(|s| s.name == "chat model").collect();
        assert_eq!(chats.len(), 1);
        let chat = chats[0];
        assert_eq!(chat.status, Status::error(""));
        assert!(attr(chat, "gen_ai.response.time_to_first_chunk").is_none());
        assert_eq!(
            attr(chat, "error.type"),
            Some(&Value::from("authentication"))
        );
        assert_private(&spans);
    }

    /// The `gen_ai.request.attempt` events of a span, as the attempt number
    /// and the `error.type` each carries.
    fn attempt_events(span: &opentelemetry_sdk::trace::SpanData) -> Vec<(i64, Option<String>)> {
        span.events
            .iter()
            .filter(|event| event.name == "gen_ai.request.attempt")
            .map(|event| {
                let value = |key: &str| {
                    event
                        .attributes
                        .iter()
                        .find(|attribute| attribute.key.as_str() == key)
                        .map(|attribute| attribute.value.clone())
                };
                let attempt = match value("gen_ai.request.attempt") {
                    Some(opentelemetry::Value::I64(attempt)) => attempt,
                    other => panic!("attempt event without a number: {other:?}"),
                };
                (attempt, value("error.type").map(|v| v.to_string()))
            })
            .collect()
    }

    #[tokio::test]
    async fn compaction_request_retried_once() {
        use opentelemetry::Value;
        use querymt_agent_services::tools::trace_support::genai_trace::{
            assert_private, attr, capture,
        };
        let provider = ScriptedProvider::new(
            true,
            vec![
                Attempt::SetupError(LLMError::HttpStatus {
                    status_code: 503,
                    message: "SECRET_ERROR".into(),
                    retry_after_secs: Some(0),
                }),
                Attempt::Stream(
                    structured_summary("SECRET_RESPONSE", true)
                        .into_iter()
                        .map(Ok)
                        .collect(),
                ),
            ],
        );
        let config = config_with(|_| {}).await;
        let (result, spans) = capture(SessionCompaction::new().process(
            &[MessageFixture::user_message(
                "m1",
                "compaction-session",
                "SECRET_PROMPT",
            )],
            None,
            None,
            &CompactionDispatch {
                provider: provider.clone(),
                config: &config,
                cancel: &CancellationToken::new(),
                ledger: &test_ledger().await,
            },
        ))
        .await;

        assert_eq!(result.unwrap().summary, "SECRET_RESPONSE");
        assert_eq!(provider.calls.load(Ordering::Relaxed), 2);
        let chat = spans.iter().find(|span| span.name == "chat model").unwrap();
        assert_eq!(attr(chat, "gen_ai.request.attempt"), Some(&Value::I64(2)));
        // One event per attempt that ended: the first failed (an HTTP status
        // is a provider error, never its message), the second succeeded.
        assert_eq!(
            attempt_events(chat),
            vec![(1, Some("provider_error".to_string())), (2, None)]
        );
        assert_private(&spans);
    }

    #[tokio::test]
    async fn compaction_request_without_a_retry() {
        use opentelemetry::Value;
        use querymt_agent_services::tools::trace_support::genai_trace::{
            assert_private, attr, capture,
        };
        let provider = ScriptedProvider::new(
            true,
            vec![Attempt::Stream(
                structured_summary("SECRET_RESPONSE", true)
                    .into_iter()
                    .map(Ok)
                    .collect(),
            )],
        );
        let config = config_with(|_| {}).await;
        let (result, spans) = capture(SessionCompaction::new().process(
            &[MessageFixture::user_message(
                "m1",
                "compaction-session",
                "SECRET_PROMPT",
            )],
            None,
            None,
            &CompactionDispatch {
                provider,
                config: &config,
                cancel: &CancellationToken::new(),
                ledger: &test_ledger().await,
            },
        ))
        .await;

        assert!(result.is_ok());
        let chat = spans.iter().find(|span| span.name == "chat model").unwrap();
        assert_eq!(attr(chat, "gen_ai.request.attempt"), Some(&Value::I64(1)));
        assert_eq!(attempt_events(chat), vec![(1, None)]);
        assert_private(&spans);
    }

    /// A reply rejected as a summary is not a successful attempt: its event
    /// carries an `error.type`, and the next summary attempt follows.
    #[tokio::test]
    async fn rejected_summary_attempt_is_reported_as_rejected_output() {
        use querymt_agent_services::tools::trace_support::genai_trace::{assert_private, capture};
        let provider = ScriptedProvider::new(
            true,
            vec![
                Attempt::Stream(structured_summary(" ", true).into_iter().map(Ok).collect()),
                Attempt::Stream(
                    structured_summary("SECRET_RESPONSE", true)
                        .into_iter()
                        .map(Ok)
                        .collect(),
                ),
            ],
        );
        let config = config_with(|config| {
            config.execution_policy.compaction.retry.max_retries = 1;
            config.execution_policy.compaction.retry.initial_backoff_ms = 0;
        })
        .await;
        let (result, spans) = capture(SessionCompaction::new().process(
            &[MessageFixture::user_message(
                "m1",
                "compaction-session",
                "SECRET_PROMPT",
            )],
            None,
            None,
            &CompactionDispatch {
                provider: provider.clone(),
                config: &config,
                cancel: &CancellationToken::new(),
                ledger: &test_ledger().await,
            },
        ))
        .await;

        assert_eq!(result.unwrap().summary, "SECRET_RESPONSE");
        let chat = spans.iter().find(|span| span.name == "chat model").unwrap();
        assert_eq!(
            attempt_events(chat),
            vec![(1, Some("output_rejected".to_string())), (2, None)]
        );
        assert_private(&spans);
    }

    #[tokio::test]
    async fn cancel_before_the_request_is_polled_reports_it_unsent() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let polled = AtomicUsize::new(0);
        let request = std::future::poll_fn(|_| {
            polled.fetch_add(1, Ordering::Relaxed);
            std::task::Poll::<()>::Pending
        });
        assert_eq!(unless_cancelled(&cancel, request).await, Err(false));
        assert_eq!(polled.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn cancel_after_the_request_was_polled_reports_it_possibly_sent() {
        let cancel = CancellationToken::new();
        let request = std::future::pending::<()>();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            trigger.cancel();
        });
        assert_eq!(unless_cancelled(&cancel, request).await, Err(true));
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
        calls: AtomicUsize,
        delay: Duration,
        clock: Arc<std::sync::Mutex<(Option<std::time::Instant>, Option<f64>)>>,
        /// When each request reached the provider, on the tokio clock (paused
        /// in the tests that assert a wait).
        sent: std::sync::Mutex<Vec<tokio::time::Instant>>,
    }

    impl ScriptedProvider {
        fn new(streaming: bool, attempts: Vec<Attempt>) -> Arc<Self> {
            Arc::new(Self {
                streaming,
                attempts: tokio::sync::Mutex::new(attempts.into()),
                calls: AtomicUsize::new(0),
                delay: Duration::ZERO,
                clock: Default::default(),
                sent: Default::default(),
            })
        }

        async fn next(&self) -> Attempt {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.sent.lock().unwrap().push(tokio::time::Instant::now());
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

        async fn chat(&self, _: &ChatCall<'_>) -> std::result::Result<ChatOutput, LLMError> {
            let Attempt::Output(output) = self.next().await else {
                panic!("expected non-streaming call")
            };
            Ok(*output)
        }

        async fn chat_stream(
            &self,
            _: &ChatCall<'_>,
        ) -> std::result::Result<querymt::chat::ChatStream, LLMError> {
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

    /// An agent configuration for the compaction under test. The summary
    /// budget is `max_retries`, no wait is longer than zero seconds and
    /// there is no jitter, so a test that does not assert a wait does not
    /// wait. Dereferences to the [`AgentConfig`].
    struct TestConfig {
        config: crate::agent::agent_config::AgentConfig,
        _dir: tempfile::TempDir,
    }

    impl std::ops::Deref for TestConfig {
        type Target = crate::agent::agent_config::AgentConfig;

        fn deref(&self) -> &Self::Target {
            &self.config
        }
    }

    async fn config_with(
        edit: impl FnOnce(&mut crate::agent::agent_config::AgentConfig),
    ) -> TestConfig {
        let (registry, dir) =
            crate::test_utils::helpers::empty_plugin_registry().expect("plugin registry");
        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap(),
        );
        let mut config = crate::agent::agent_config_builder::AgentConfigBuilder::new(
            Arc::new(registry),
            storage,
            querymt::LLMParams::new().provider("mock").model("mock"),
        )
        .build();
        let policy = &mut config.execution_policy;
        policy.rate_limit.jitter_ratio = 0.0;
        policy.rate_limit.default_wait_secs = 0;
        policy.rate_limit.transport_wait_secs = 0;
        policy.compaction.retry.initial_backoff_ms = 0;
        policy.compaction.retry.backoff_multiplier = 1.0;
        edit(&mut config);
        TestConfig { config, _dir: dir }
    }

    /// A configuration whose rejected-summary budget is `max_retries`.
    async fn test_config(max_retries: usize) -> TestConfig {
        config_with(|config| config.execution_policy.compaction.retry.max_retries = max_retries)
            .await
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

    async fn process_script(
        provider: Arc<ScriptedProvider>,
        max_retries: usize,
    ) -> Result<CompactionResult> {
        SessionCompaction::new()
            .process(
                &[],
                None,
                None,
                &CompactionDispatch {
                    provider,
                    config: &*test_config(max_retries).await,
                    cancel: &CancellationToken::new(),
                    ledger: &test_ledger().await,
                },
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
                None,
                None,
                &CompactionDispatch {
                    provider: provider.clone(),
                    config: &*test_config(1).await,
                    cancel: &CancellationToken::new(),
                    ledger: &test_ledger().await,
                },
            )
            .await
            .unwrap();
        assert_eq!(result.summary, "Valid continuation summary");
        assert_eq!(provider.call_count(), 2);
    }

    fn task_with(
        status: crate::session::domain::TaskStatus,
        deliverable: Option<&str>,
        criteria: Option<&str>,
    ) -> crate::session::domain::Task {
        let now = time::OffsetDateTime::now_utc();
        crate::session::domain::Task {
            id: 1,
            public_id: "task-7".into(),
            session_id: 1,
            kind: crate::session::domain::TaskKind::Finite,
            status,
            expected_deliverable: deliverable.map(str::to_owned),
            acceptance_criteria: criteria.map(str::to_owned),
            revision: 1,
            creation_key: None,
            completion_evidence: None,
            completed_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn compaction_with_an_active_task_carries_its_identifier_and_criteria() {
        use crate::session::domain::TaskStatus;
        let task = task_with(
            TaskStatus::Active,
            Some("a parser fix"),
            Some("all tests pass"),
        );

        let section = active_task_section(Some(&task)).expect("task section");

        assert!(section.contains("task-7"));
        assert!(section.contains("Deliverable: a parser fix"));
        assert!(section.contains("Acceptance criteria: all tests pass"));
    }

    #[test]
    fn compaction_without_an_active_task_has_no_task_section() {
        use crate::session::domain::TaskStatus;
        assert!(active_task_section(None).is_none());
        // Finished, and active-but-empty tasks add nothing either.
        let done = task_with(TaskStatus::Done, Some("x"), Some("y"));
        assert!(active_task_section(Some(&done)).is_none());
        let empty = task_with(TaskStatus::Active, None, None);
        assert!(active_task_section(Some(&empty)).is_none());
    }

    #[tokio::test]
    async fn compaction_appends_verbatim_user_prompts_after_the_summary() {
        use crate::acp::protocol::{ContentBlock, TextContent};

        let prompt = |id: &str, text: &str| AgentMessage {
            id: id.into(),
            session_id: "s1".into(),
            role: ChatRole::User,
            parts: vec![MessagePart::Prompt {
                blocks: vec![ContentBlock::Text(TextContent::new(text))],
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        let messages = vec![
            prompt("1", "Refactor the parser, keep the public API"),
            MessageFixture::assistant_message("2", "s1", "ok"),
            prompt("3", "Also add tests"),
        ];
        let provider = Arc::new(MockCompactionProvider::new(vec![Ok("Model brief".into())]));
        let result = SessionCompaction::new()
            .process(
                &messages,
                None,
                None,
                &CompactionDispatch {
                    provider,
                    config: &*test_config(0).await,
                    cancel: &CancellationToken::new(),
                    ledger: &test_ledger().await,
                },
            )
            .await
            .unwrap();

        assert!(result.summary.starts_with("Model brief"));
        let first = result.summary.find("Refactor the parser").unwrap();
        let second = result.summary.find("Also add tests").unwrap();
        assert!(first < second, "prompts are oldest first");
        assert!(result.summary.contains("<original_task"));
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
            // A stream that closes early is a lost connection, which the
            // shared retry resends: every request of its budget gets the same
            // answer, and none of them is a summary.
            let provider = ScriptedProvider::new(
                true,
                (0..3)
                    .map(|_| Attempt::Stream(chunks.iter().cloned().map(Ok).collect()))
                    .collect(),
            );
            let error = process_script(provider.clone(), 0).await.unwrap_err();
            assert!(error.to_string().contains("without a terminal event"));
            assert_eq!(provider.calls.load(Ordering::Relaxed), 3);
        }
    }

    #[tokio::test]
    async fn a_stream_closed_before_its_terminal_event_recovers_on_the_next_request() {
        let provider = ScriptedProvider::new(
            true,
            vec![
                Attempt::Stream(vec![Ok(StreamChunk::Text("partial".into()))]),
                Attempt::Stream(
                    structured_summary("Valid continuation summary", true)
                        .into_iter()
                        .map(Ok)
                        .collect(),
                ),
            ],
        );
        assert_eq!(
            process_script(provider.clone(), 0).await.unwrap().summary,
            "Valid continuation summary"
        );
        assert_eq!(provider.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn genai_compaction_timeout_is_a_failed_attempt_and_is_not_retried() {
        use querymt_agent_services::tools::trace_support::genai_trace::capture;
        let timeout = || {
            Attempt::SetupError(LLMError::Transport {
                kind: querymt::error::TransportErrorKind::Timeout,
                message: "network timeout".into(),
            })
        };
        let provider = ScriptedProvider::new(true, vec![timeout(), timeout()]);
        let (result, spans) = capture(process_script(provider.clone(), 0)).await;

        assert!(result.unwrap_err().to_string().contains("timeout"));
        assert_eq!(provider.calls.load(Ordering::Relaxed), 1);
        let chat = spans
            .iter()
            .find(|span| span.name == "chat model")
            .unwrap_or_else(|| {
                panic!(
                    "no chat span in {:?}",
                    spans.iter().map(|s| s.name.clone()).collect::<Vec<_>>()
                )
            });
        let events = attempt_events(chat);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].0, 1);
        assert!(events[0].1.is_some(), "the attempt failed: {events:?}");
    }

    #[tokio::test]
    async fn cancelling_stops_a_stream_that_is_blocked_before_its_terminal_chunk() {
        let provider = ScriptedProvider::new(
            true,
            vec![Attempt::StreamWithPendingTail(vec![Ok(
                StreamChunk::Usage(Default::default()),
            )])],
        );
        let cancel = CancellationToken::new();
        let config = test_config(0).await;
        let ledger = test_ledger().await;
        // Cancel once the request is in flight, not on a timer: the stream
        // never ends, so the only way out is the cancellation.
        let canceller = cancel.clone();
        let watched = provider.clone();
        tokio::spawn(async move {
            while watched.calls.load(Ordering::Relaxed) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            canceller.cancel();
        });
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            SessionCompaction::new().process(
                &[],
                None,
                None,
                &CompactionDispatch {
                    provider: provider.clone(),
                    config: &config,
                    cancel: &cancel,
                    ledger: &ledger,
                },
            ),
        )
        .await
        .expect("cancelling must not wait for the stream");

        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert_eq!(provider.calls.load(Ordering::Relaxed), 1, "no retry");
    }

    #[tokio::test]
    async fn failed_stream_retry_discards_previous_partial_text() {
        let provider = ScriptedProvider::new(
            true,
            vec![
                Attempt::Stream(vec![
                    Ok(StreamChunk::Text("discard me".into())),
                    Err(LLMError::Transport {
                        kind: querymt::error::TransportErrorKind::ConnectionClosed,
                        message: "transport failed".into(),
                    }),
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
                    None,
                    None,
                    &CompactionDispatch {
                        provider: Arc::new(mock),
                        config: &*test_config(3).await,
                        cancel: &CancellationToken::new(),
                        ledger: &test_ledger().await,
                    },
                )
                .await
        }
    }

    /// The shared estimate of a stored history.
    fn estimate_tokens(messages: &[AgentMessage], max_prompt_bytes: Option<usize>) -> usize {
        crate::session::pruning::estimate_history_tokens(messages, max_prompt_bytes, false).unwrap()
    }

    // ========================================================================
    // Helper Functions (backwards compatibility)
    // ========================================================================

    fn make_text_message(id: &str, session_id: &str, role: ChatRole, text: &str) -> AgentMessage {
        MessageFixture::text_message(id, session_id, role, text)
    }

    fn make_compaction_message(id: &str, session_id: &str, summary: &str) -> AgentMessage {
        MessageFixture::compaction_message(id, session_id, summary)
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
        let structured_estimate = estimate_tokens(&[structured], None);
        let legacy_estimate = estimate_tokens(&[legacy_equivalent], None);
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
        let messages = MessageFixture::simple_conversation("s1");

        let tokens = estimate_tokens(&messages, None);
        assert!(tokens > 0, "Should count tokens for text messages");
    }

    #[test]
    fn test_estimate_messages_tokens_tool_results() {
        let messages = MessageFixture::conversation_with_tools("s1");

        let tokens = estimate_tokens(&messages, None);
        assert!(tokens > 0, "Should count tokens for tool results");
    }

    #[test]
    fn test_estimate_messages_tokens_mixed_parts() {
        let messages = vec![
            MessageFixture::user_message("1", "s1", "User request"),
            MessageFixture::reasoning_message("2", "s1", "Let me think about this"),
            MessageFixture::tool_result_message("3", "s1", "c1", "Tool output"),
            MessageFixture::assistant_message("4", "s1", "Final response"),
        ];

        let tokens = estimate_tokens(&messages, None);
        assert!(tokens > 0, "Should count tokens for all part types");
    }

    #[test]
    fn notice_is_estimated_like_text_of_the_same_content() {
        let content = "The agent mode changed. ".repeat(20);
        let text = MessageFixture::user_message("1", "s1", &content);
        let mut notice = text.clone();
        notice.parts = vec![MessagePart::Notice {
            source: querymt_agent_core::model::NoticeSource::AgentMode,
            content: content.clone(),
        }];

        let notice_tokens = estimate_tokens(&[notice], None);
        assert!(notice_tokens > 0);
        assert_eq!(notice_tokens, estimate_tokens(&[text], None));
    }

    #[test]
    fn test_estimate_messages_tokens_empty() {
        let messages: Vec<AgentMessage> = vec![];

        let tokens = estimate_tokens(&messages, None);
        assert_eq!(tokens, 0, "Empty messages should have 0 tokens");
    }

    #[test]
    fn test_estimate_messages_tokens_long_conversation() {
        let messages = MessageFixture::long_conversation("s1");

        let tokens = estimate_tokens(&messages, None);
        assert!(
            tokens > 500,
            "Long conversation should have significant token count"
        );
    }

    #[test]
    fn test_compaction_prompt_without_instructions_is_unchanged() {
        assert_eq!(compaction_prompt(None), COMPACTION_PROMPT);
        assert_eq!(compaction_prompt(Some("   \n")), COMPACTION_PROMPT);
    }

    #[test]
    fn test_compaction_prompt_includes_user_focus() {
        let prompt = compaction_prompt(Some("  focus on the API changes  "));
        assert!(prompt.starts_with(COMPACTION_PROMPT));
        assert!(prompt.contains("<user_focus>\nfocus on the API changes\n</user_focus>"));
    }

    #[test]
    fn test_build_compaction_messages_with_instructions_carries_focus() {
        let messages = MessageFixture::simple_conversation("s1");
        let chat = SessionCompaction::new()
            .build_compaction_messages_with_instructions(
                &messages,
                None,
                Some("ignore the test refactoring"),
            )
            .unwrap();
        let last = chat.last().unwrap();
        assert!(last.text().contains("ignore the test refactoring"));
    }

    #[test]
    fn test_build_compaction_messages_appends_prompt() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::simple_conversation("s1");

        let chat_messages = fixture
            .service
            .build_compaction_messages(&messages, None)
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
    fn compaction_request_leaves_out_tool_baselines_and_changes() {
        let fixture = CompactionFixture::new();
        let mut messages = MessageFixture::simple_conversation("s1");
        let tool = querymt::chat::Tool {
            tool_type: "function".into(),
            function: querymt::chat::FunctionTool {
                name: "deploy".into(),
                description: "Deploy".into(),
                parameters: serde_json::json!({"type": "object"}),
                strict: None,
            },
        };
        let record = |part: MessagePart| {
            let mut message = MessageFixture::user_message("r", "s1", "");
            message.parts = vec![part];
            message
        };
        messages.insert(
            1,
            record(MessagePart::ToolBaseline {
                tools: vec![tool.clone()],
                origin: "anthropic/claude-sonnet-5-5".into(),
            }),
        );
        messages.push(record(MessagePart::ToolChange {
            added: vec![tool],
            removed: vec!["old".into()],
        }));

        let chat = fixture
            .service
            .build_compaction_messages(&messages, None)
            .unwrap();

        assert_eq!(chat.len(), messages.len() - 2 + 1, "records are left out");
        assert!(!chat.iter().any(|m| m.has_tool_change()));
        // A provider that rejects tool-change messages accepts the request.
        querymt::chat::reject_tool_changes("acme", &chat).unwrap();
    }

    #[test]
    fn test_build_compaction_messages_preserves_roles() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::simple_conversation("s1");

        let chat_messages = fixture
            .service
            .build_compaction_messages(&messages, None)
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
            .build_compaction_messages(&[message], None)
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
                None,
                None,
                &CompactionDispatch {
                    provider: provider.clone(),
                    config: &*test_config(3).await,
                    cancel: &CancellationToken::new(),
                    ledger: &test_ledger().await,
                },
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

        let original_tokens = estimate_tokens(&messages, None);
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

    /// A retryable provider failure that the provider answered.
    fn unavailable() -> LLMError {
        LLMError::HttpStatus {
            status_code: 503,
            message: "service unavailable".into(),
            retry_after_secs: None,
        }
    }

    #[tokio::test]
    async fn test_process_retries_on_llm_error() {
        let service = SessionCompaction::new();
        let messages = MessageFixture::simple_conversation("s1");

        // Fails twice with a retryable error, then succeeds: exactly the
        // three requests `rate_limit.max_retries` (3) allows.
        let mock = MockCompactionProvider::new(vec![
            Err(unavailable()),
            Err(unavailable()),
            Ok("recovered summary".to_string()),
        ]);
        let mock_arc = Arc::new(mock);

        let result = service
            .process(
                &messages,
                None,
                None,
                &CompactionDispatch {
                    provider: mock_arc.clone(),
                    config: &*test_config(3).await,
                    cancel: &CancellationToken::new(),
                    ledger: &test_ledger().await,
                },
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

        // Create a mock that always fails with a retryable error
        let mock = MockCompactionProvider::new(vec![
            Err(unavailable()),
            Err(unavailable()),
            Err(unavailable()),
            Err(unavailable()),
        ]);
        let mock_arc = Arc::new(mock);

        let result = service
            .process(
                &messages,
                None,
                None,
                &CompactionDispatch {
                    provider: mock_arc.clone(),
                    config: &*test_config(3).await,
                    cancel: &CancellationToken::new(),
                    ledger: &test_ledger().await,
                },
            )
            .await;

        assert!(result.is_err(), "should fail after max retries");
        // The default `rate_limit.max_retries` is 3 requests in total; the
        // summary budget (`compaction.retry.max_retries`) does not apply.
        assert_eq!(mock_arc.call_count(), 3);
    }

    #[tokio::test]
    async fn test_process_with_custom_retry_config() {
        let service = SessionCompaction::new();
        let messages = MessageFixture::simple_conversation("s1");

        // Provider errors follow `rate_limit.max_retries`; the compaction
        // budget of 1 does not limit them.
        let config = config_with(|config| {
            config.execution_policy.rate_limit.max_retries = 2;
            config.execution_policy.compaction.retry.max_retries = 1;
            config.execution_policy.compaction.retry.initial_backoff_ms = 10;
        })
        .await;

        let mock = MockCompactionProvider::new(vec![
            Err(unavailable()),
            Err(unavailable()),
            Err(unavailable()),
        ]);
        let mock_arc = Arc::new(mock);

        let result = service
            .process(
                &messages,
                None,
                None,
                &CompactionDispatch {
                    provider: mock_arc.clone(),
                    config: &config,
                    cancel: &CancellationToken::new(),
                    ledger: &test_ledger().await,
                },
            )
            .await;

        assert!(result.is_err());
        // With rate_limit.max_retries=2, there are 2 requests in total.
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
            .build_compaction_messages(&messages, None)
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

    // ========================================================================
    // is_request_timeout tests
    // ========================================================================

    #[test]
    fn test_mesh_timeout_network_timeout() {
        let err = LLMError::Transport {
            kind: querymt::error::TransportErrorKind::Timeout,
            message: "network timeout".to_string(),
        };
        assert!(
            is_request_timeout(&err),
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
            is_request_timeout(&err),
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
            !is_request_timeout(&err),
            "dial failure should NOT be classified as mesh timeout"
        );
    }

    #[test]
    fn test_mesh_timeout_generic_error_is_not_timeout() {
        let err = LLMError::GenericError("some other error".to_string());
        assert!(
            !is_request_timeout(&err),
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
            !is_request_timeout(&err),
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
                None,
                None,
                &CompactionDispatch {
                    provider: mock_arc.clone(),
                    config: &*test_config(3).await,
                    cancel: &CancellationToken::new(),
                    ledger: &test_ledger().await,
                },
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

    mod accounting_tests {
        use super::*;
        use querymt::providers::{BillingClass, ModelPricing};
        use querymt_agent_services::accounting::{AccountingReason, UsageDisposition};

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
            Arc<querymt_agent_services::accounting::SessionAccounting>,
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
                    None,
                    None,
                    &CompactionDispatch {
                        provider,
                        // One request per summary attempt: a provider error is not
                        // retried, so each scripted reply is one accounted attempt.
                        config: &*config_with(|config| {
                            config.execution_policy.compaction.retry.max_retries = retries;
                            config.execution_policy.rate_limit.max_retries = 1;
                        })
                        .await,
                        cancel: &CancellationToken::new(),
                        ledger: &ledger,
                    },
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
            let compaction = accounting.read(|r| {
                r.purpose(querymt_agent_services::accounting::AttemptPurpose::Compaction)
            });
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
                .process(
                    &history,
                    None,
                    None,
                    &CompactionDispatch {
                        provider,
                        config: &*test_config(0).await,
                        cancel: &CancellationToken::new(),
                        ledger: &ledger,
                    },
                )
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
                .process(
                    &history,
                    None,
                    None,
                    &CompactionDispatch {
                        provider,
                        config: &*test_config(0).await,
                        cancel: &CancellationToken::new(),
                        ledger: &ledger,
                    },
                )
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
            let foreign = querymt_agent_services::accounting::DispatchLedger {
                operation: Arc::new(querymt_agent_services::accounting::AttemptOperation::new(
                    Arc::new(querymt_agent_services::accounting::AttemptRecorder::new(
                        querymt_agent_services::session_event_stream::SessionEventStream::new(
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
                            Arc::new(querymt_agent_services::event_fanout::EventFanout::new()),
                        ),
                    )),
                    &querymt_agent_services::accounting::CycleAccounting::with_id(
                        querymt_agent_services::accounting::SessionAccounting::hydrate(
                            "compaction-session",
                            {
                                use crate::session::backend::StorageBackend;
                                crate::session::sqlite_storage::SqliteStorage::connect(
                                    ":memory:".into(),
                                )
                                .await
                                .unwrap()
                                .event_journal()
                            },
                        )
                        .await
                        .unwrap(),
                        "c",
                    ),
                    querymt_agent_services::accounting::AttemptPurpose::Compaction,
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
                    None,
                    None,
                    &CompactionDispatch {
                        provider: provider.clone(),
                        config: &*test_config(2).await,
                        cancel: &CancellationToken::new(),
                        ledger: &foreign,
                    },
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

    /// The retry rules of `session-compaction` (change `unify-model-call-path`):
    /// provider errors follow `[agent.execution.rate_limit]`, rejected
    /// summaries follow `[agent.execution.compaction.retry]`. Every test goes
    /// through `compact`, the one place that knows the signature of
    /// `process`.
    mod retry_rules_tests {
        use super::*;
        use crate::agent::agent_config::AgentConfig;
        use querymt::error::{ProviderErrorKind, ProviderFailure, TransportErrorKind};
        use tokio_util::sync::CancellationToken;

        /// One compaction of a one-message history. The clock is paused after
        /// the ledger is built, so the waits of the retry rules cost no real
        /// time and are measured on `ScriptedProvider::sent`.
        async fn compact(
            provider: &Arc<ScriptedProvider>,
            config: &AgentConfig,
            cancel: &CancellationToken,
        ) -> Result<CompactionResult> {
            let ledger = test_ledger().await;
            tokio::time::pause();
            compact_with(provider, config, cancel, &ledger).await
        }

        /// [`compact`] over a ledger the test built, on a clock the test
        /// has already paused.
        async fn compact_with(
            provider: &Arc<ScriptedProvider>,
            config: &AgentConfig,
            cancel: &CancellationToken,
            ledger: &DispatchLedger,
        ) -> Result<CompactionResult> {
            let history = [MessageFixture::user_message(
                "m1",
                "compaction-session",
                "hello",
            )];
            SessionCompaction::new()
                .process(
                    &history,
                    None,
                    None,
                    &CompactionDispatch {
                        provider: provider.clone(),
                        config,
                        cancel,
                        ledger,
                    },
                )
                .await
        }

        fn summary(text: &str) -> Attempt {
            Attempt::Stream(structured_summary(text, true).into_iter().map(Ok).collect())
        }

        fn overloaded(retry_after_secs: u64) -> Attempt {
            Attempt::SetupError(
                ProviderFailure::new(ProviderErrorKind::ServerOverloaded, "overloaded")
                    .with_retry_after_secs(Some(retry_after_secs))
                    .into(),
            )
        }

        fn refused() -> Attempt {
            Attempt::SetupError(LLMError::Transport {
                kind: TransportErrorKind::ConnectionRefused,
                message: "connection refused".into(),
            })
        }

        /// Time between request `index` and the next one.
        fn gap(provider: &ScriptedProvider, index: usize) -> Duration {
            let sent = provider.sent.lock().unwrap();
            sent[index + 1] - sent[index]
        }

        fn calls(provider: &ScriptedProvider) -> usize {
            provider.calls.load(Ordering::Relaxed)
        }

        #[tokio::test]
        async fn overloaded_provider_during_compaction() {
            let config = config_with(|config| {
                // A fixed backoff that the provider's own delay must override.
                config.execution_policy.compaction.retry.initial_backoff_ms = 60_000;
            })
            .await;
            let provider =
                ScriptedProvider::new(true, vec![overloaded(2), summary("a real summary")]);

            let result = compact(&provider, &config, &CancellationToken::new()).await;

            assert_eq!(result.unwrap().summary, "a real summary");
            assert_eq!(calls(&provider), 2);
            let wait = gap(&provider, 0);
            assert!(
                wait >= Duration::from_secs(2) && wait < Duration::from_secs(3),
                "the second request follows the 2 s the provider named, got {wait:?}"
            );
        }

        /// The retry events of a compaction belong to the session that
        /// initiated it (the ledger's), whatever session the history names.
        #[tokio::test]
        async fn retry_events_go_to_the_initiating_session_not_the_histories() {
            use querymt_agent_core::events::{AgentEventKind, EventEnvelope};
            let config = config_with(|_| {}).await;
            let mut events = config.subscribe_events();
            let provider = ScriptedProvider::new(true, vec![overloaded(0), summary("a summary")]);
            let ledger = test_ledger().await;
            assert_eq!(ledger.operation.session_id(), "compaction-session");
            tokio::time::pause();
            let history = [MessageFixture::user_message(
                "m1",
                "history-session",
                "hello",
            )];

            SessionCompaction::new()
                .process(
                    &history,
                    None,
                    None,
                    &CompactionDispatch {
                        provider: provider.clone(),
                        config: &config,
                        cancel: &CancellationToken::new(),
                        ledger: &ledger,
                    },
                )
                .await
                .unwrap();

            let mut sessions = Vec::new();
            while let Ok(envelope) = events.try_recv() {
                if let EventEnvelope::Durable(event) = envelope
                    && matches!(
                        event.kind,
                        AgentEventKind::LlmRetryWait { .. }
                            | AgentEventKind::LlmRetryResume { .. }
                            | AgentEventKind::RateLimited { .. }
                            | AgentEventKind::RateLimitResume { .. }
                    )
                {
                    sessions.push(event.session_id);
                }
            }
            assert_eq!(
                sessions,
                ["compaction-session", "compaction-session"],
                "one wait and one resume, both on the initiating session"
            );
        }

        #[tokio::test]
        async fn connection_failure_during_compaction() {
            let config = config_with(|config| {
                config.execution_policy.rate_limit.transport_wait_secs = 7;
                config.execution_policy.compaction.retry.initial_backoff_ms = 60_000;
            })
            .await;
            let provider = ScriptedProvider::new(true, vec![refused(), summary("a real summary")]);

            let result = compact(&provider, &config, &CancellationToken::new()).await;

            assert_eq!(result.unwrap().summary, "a real summary");
            let wait = gap(&provider, 0);
            assert!(
                wait >= Duration::from_secs(7) && wait < Duration::from_secs(8),
                "the wait is transport_wait_secs, not the compaction backoff, got {wait:?}"
            );
        }

        #[tokio::test]
        async fn rejected_credential_during_compaction() {
            let config = config_with(|_| {}).await;
            let provider = ScriptedProvider::new(
                true,
                vec![
                    Attempt::SetupError(LLMError::AuthError("bad key".into())),
                    summary("never requested"),
                ],
            );

            let result = compact(&provider, &config, &CancellationToken::new()).await;

            assert!(result.is_err());
            assert_eq!(
                calls(&provider),
                1,
                "an authentication error is not retried"
            );
        }

        #[tokio::test]
        async fn provider_errors_on_every_compaction_attempt() {
            let config = config_with(|config| {
                config.execution_policy.rate_limit.max_retries = 2;
            })
            .await;
            let provider = ScriptedProvider::new(
                true,
                vec![overloaded(1), overloaded(1), overloaded(1), overloaded(1)],
            );

            let result = compact(&provider, &config, &CancellationToken::new()).await;

            assert!(
                result.is_err(),
                "no summary, so nothing replaces the history"
            );
            assert_eq!(
                calls(&provider),
                2,
                "rate_limit.max_retries is the number of requests"
            );
        }

        #[tokio::test]
        async fn provider_error_with_the_summary_budget_at_zero() {
            let config = config_with(|config| {
                config.execution_policy.compaction.retry.max_retries = 0;
            })
            .await;
            let provider =
                ScriptedProvider::new(true, vec![overloaded(1), summary("a real summary")]);

            let result = compact(&provider, &config, &CancellationToken::new()).await;

            assert_eq!(result.unwrap().summary, "a real summary");
            assert_eq!(calls(&provider), 2);
        }

        #[tokio::test]
        async fn timeout_during_compaction() {
            let config = config_with(|_| {}).await;
            let provider = ScriptedProvider::new(
                true,
                vec![
                    Attempt::SetupError(LLMError::Transport {
                        kind: TransportErrorKind::Timeout,
                        message: "network timeout".into(),
                    }),
                    summary("never requested"),
                ],
            );

            let result = compact(&provider, &config, &CancellationToken::new()).await;

            let error = result.unwrap_err().to_string();
            assert!(error.contains("timeout"), "names the timeout: {error}");
            assert_eq!(calls(&provider), 1, "a timeout is not retried");
        }

        #[tokio::test]
        async fn empty_summary_is_retried() {
            let config = config_with(|config| {
                config.execution_policy.compaction.retry.initial_backoff_ms = 5_000;
            })
            .await;
            let provider =
                ScriptedProvider::new(true, vec![summary(""), summary("a real summary")]);

            let result = compact(&provider, &config, &CancellationToken::new()).await;

            assert_eq!(result.unwrap().summary, "a real summary");
            let wait = gap(&provider, 0);
            assert!(
                wait >= Duration::from_secs(5) && wait < Duration::from_secs(6),
                "the second request follows initial_backoff_ms, got {wait:?}"
            );
        }

        #[tokio::test]
        async fn summary_rejected_on_every_attempt() {
            let config = config_with(|config| {
                config.execution_policy.compaction.retry.max_retries = 1;
            })
            .await;
            let provider = ScriptedProvider::new(true, vec![summary(""), summary("  ")]);

            let result = compact(&provider, &config, &CancellationToken::new()).await;

            assert!(result.is_err());
            assert_eq!(calls(&provider), 2, "max_retries = 1 is two requests");
        }

        #[tokio::test]
        async fn each_summary_attempt_has_its_own_provider_retries() {
            let config = config_with(|config| {
                config.execution_policy.compaction.retry.max_retries = 1;
                config.execution_policy.rate_limit.max_retries = 3;
            })
            .await;
            // Attempt 1 is rejected; attempt 2 meets one provider error first.
            let provider = ScriptedProvider::new(
                true,
                vec![summary(""), overloaded(3), summary("a real summary")],
            );

            let result = compact(&provider, &config, &CancellationToken::new()).await;

            assert_eq!(result.unwrap().summary, "a real summary");
            assert_eq!(calls(&provider), 3);
            let wait = gap(&provider, 1);
            assert!(
                wait >= Duration::from_secs(3) && wait < Duration::from_secs(4),
                "the provider error waits as a model request does, got {wait:?}"
            );
        }

        /// Cancel `cancel` one second (paused clock) into a compaction whose
        /// first wait is far longer, and return what the compaction did.
        async fn compact_cancelled_after_one_second(
            provider: &Arc<ScriptedProvider>,
            config: &AgentConfig,
        ) -> Result<CompactionResult> {
            let ledger = test_ledger().await;
            tokio::time::pause();
            let started = tokio::time::Instant::now();
            let cancel = CancellationToken::new();
            let (result, ()) =
                tokio::join!(compact_with(provider, config, &cancel, &ledger), async {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    cancel.cancel();
                });
            // The wait is abandoned when the token fires, not sat out and
            // then refused (the 600 s wait would otherwise be on the clock).
            let waited = started.elapsed();
            assert!(
                waited < Duration::from_secs(5),
                "the compaction stopped waiting at the cancel, not after {waited:?}"
            );
            // The test runs two cases, each with its own paused clock.
            tokio::time::resume();
            result
        }

        #[tokio::test]
        async fn cancelled_while_compaction_waits_to_retry() {
            // During the wait before a provider-error retry (the provider
            // names 600 s).
            let config = config_with(|_| {}).await;
            let provider = ScriptedProvider::new(true, vec![overloaded(600), summary("never")]);
            let result = compact_cancelled_after_one_second(&provider, &config).await;
            let error = result.expect_err("no summary is stored").to_string();
            assert!(error.contains("cancelled"), "{error}");
            assert_eq!(calls(&provider), 1, "no further request after the cancel");

            // During the backoff before a rejected-summary retry.
            let config = config_with(|config| {
                config.execution_policy.compaction.retry.initial_backoff_ms = 600_000;
            })
            .await;
            let provider = ScriptedProvider::new(true, vec![summary(""), summary("never")]);
            let result = compact_cancelled_after_one_second(&provider, &config).await;
            let error = result.expect_err("no summary is stored").to_string();
            assert!(error.contains("cancelled"), "{error}");
            assert_eq!(calls(&provider), 1, "no further request after the cancel");
        }
    }
}
