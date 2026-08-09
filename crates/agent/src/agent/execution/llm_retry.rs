//! LLM retry logic with rate limit handling
//!
//! This module handles retrying transient LLM failures with configurable backoff.

use crate::agent::agent_config::AgentConfig;
use crate::agent::utils::u32_from_usize;
use crate::events::AgentEventKind;
use futures_util::{Stream, StreamExt};
use log::{info, warn};
use querymt::chat::StreamChunk;
use querymt::error::LLMError;
use std::future::Future;
use std::pin::Pin;
use tokio_util::sync::CancellationToken;
use tracing::{Span, instrument};

/// Call an LLM with automatic retry for transient provider and transport errors.
///
/// Uses provider hints or exponential backoff and respects cancellation via the token.
#[instrument(
    name = "agent.llm.call_with_retry",
    skip(config, cancel_token, call_fn),
    fields(session_id = %session_id, attempt = tracing::field::Empty, retrying = tracing::field::Empty)
)]
pub(super) async fn call_with_retry<T, F, Fut>(
    config: &AgentConfig,
    session_id: &str,
    cancel_token: &CancellationToken,
    call_fn: F,
) -> Result<T, LLMError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, LLMError>>,
{
    call_with_retry_mode(config, session_id, cancel_token, call_fn).await
}

async fn call_with_retry_mode<T, F, Fut>(
    config: &AgentConfig,
    session_id: &str,
    cancel_token: &CancellationToken,
    mut call_fn: F,
) -> Result<T, LLMError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, LLMError>>,
{
    let max_attempts = config.execution_policy.rate_limit.max_attempts();
    let mut attempt = 0;
    Span::current().record("retrying", false);

    loop {
        attempt += 1;
        if cancel_token.is_cancelled() {
            return Err(LLMError::Cancelled);
        }

        match call_fn().await {
            Ok(response) => {
                Span::current().record("attempt", attempt);
                return Ok(response);
            }
            Err(error) => {
                Span::current().record("attempt", attempt);
                if !error.is_retryable() || attempt >= max_attempts {
                    return Err(error);
                }

                Span::current().record("retrying", true);
                wait_for_retry(
                    config,
                    session_id,
                    &error,
                    RetryWait {
                        attempt,
                        max_attempts,
                        wait_secs: retry_delay_secs(config, &error, attempt),
                    },
                    cancel_token,
                )
                .await?;
            }
        }
    }
}

/// Calculate a bounded retry delay, preferring a provider hint when present.
///
/// Provider hints are diagnostic input, not authority over the local policy:
/// every delay is capped by `rate_limit.max_wait_secs`.
pub(super) fn retry_delay_secs(
    config: &AgentConfig,
    error: &LLMError,
    retry_ordinal: usize,
) -> u64 {
    let policy = &config.execution_policy.rate_limit;
    if let Some(secs) = error.retry_after_secs() {
        let bounded = secs.min(policy.max_wait_secs);
        if bounded != secs {
            info!(
                "provider retry hint {}s capped to rate_limit.max_wait_secs={}s",
                secs, bounded
            );
        }
        return bounded;
    }

    let base = policy.default_wait_secs as f64;
    let multiplier = policy.backoff_multiplier;
    let calculated = base * multiplier.powi(retry_ordinal.saturating_sub(1) as i32);
    apply_jitter(calculated, policy.jitter_ratio, jitter_sample()).min(policy.max_wait_secs)
}

/// Log and emit a generic LLM retry event before waiting.
async fn emit_retry_wait(
    config: &AgentConfig,
    session_id: &str,
    error: &LLMError,
    attempt: usize,
    max_attempts: usize,
    wait_secs: u64,
) {
    let message = error
        .rate_limit_info()
        .map(|(m, _)| m)
        .unwrap_or_else(|| error.to_string());
    let started_at = time::OffsetDateTime::now_utc().unix_timestamp();

    info!(
        "Session {} LLM retry wait, attempt {}/{}, waiting {}s: {}",
        session_id, attempt, max_attempts, wait_secs, message
    );
    let attempt = u32_from_usize(attempt, "attempt", Some(session_id));
    let max_attempts = u32_from_usize(max_attempts, "max_attempts", Some(session_id));
    let kind = if error.is_rate_limited() {
        AgentEventKind::RateLimited {
            message,
            wait_secs,
            started_at,
            attempt,
            max_attempts,
        }
    } else {
        AgentEventKind::LlmRetryWait {
            message,
            wait_secs,
            started_at,
            attempt,
            max_attempts,
        }
    };
    if let Err(error) = config.emit_event_persisted(session_id, kind).await {
        warn!("failed to emit LLM retry wait event for session {session_id}: {error}");
    }
}

async fn emit_retry_resume(
    config: &AgentConfig,
    session_id: &str,
    next_attempt: usize,
    rate_limited: bool,
) {
    info!(
        "Session {} resuming after LLM retry wait, attempt {}",
        session_id, next_attempt
    );
    let attempt = u32_from_usize(next_attempt, "attempt + 1", Some(session_id));
    let kind = if rate_limited {
        AgentEventKind::RateLimitResume { attempt }
    } else {
        AgentEventKind::LlmRetryResume { attempt }
    };
    if let Err(error) = config.emit_event_persisted(session_id, kind).await {
        warn!("failed to emit LLM retry resume event for session {session_id}: {error}");
    }
}

#[derive(Clone, Copy)]
pub(super) struct RetryWait {
    pub(super) attempt: usize,
    pub(super) max_attempts: usize,
    pub(super) wait_secs: u64,
}

/// Emit the shared retry events around a cancellation-aware delay.
pub(super) async fn wait_for_retry(
    config: &AgentConfig,
    session_id: &str,
    error: &LLMError,
    retry: RetryWait,
    cancel_token: &CancellationToken,
) -> Result<(), LLMError> {
    let rate_limited = error.is_rate_limited();
    emit_retry_wait(
        config,
        session_id,
        error,
        retry.attempt,
        retry.max_attempts,
        retry.wait_secs,
    )
    .await;
    wait_for_retry_delay(retry.wait_secs, cancel_token).await?;
    emit_retry_resume(
        config,
        session_id,
        retry.attempt.saturating_add(1),
        rate_limited,
    )
    .await;
    Ok(())
}

fn apply_jitter(delay_secs: f64, ratio: f64, sample: f64) -> u64 {
    let ratio = ratio.clamp(0.0, 1.0);
    let sample = sample.clamp(0.0, 1.0);
    let factor = 1.0 - ratio + (2.0 * ratio * sample);
    (delay_secs * factor).round().max(0.0) as u64
}

fn jitter_sample() -> f64 {
    rand::random::<f64>()
}

/// Wait for a retry delay, returning a typed cancellation error if interrupted.
#[instrument(name = "agent.llm.retry_wait", skip(cancel_token), fields(wait_secs = wait_secs))]
pub(super) async fn wait_for_retry_delay(
    wait_secs: u64,
    cancel_token: &CancellationToken,
) -> Result<(), LLMError> {
    tokio::select! {
        biased;
        _ = cancel_token.cancelled() => Err(LLMError::Cancelled),
        _ = tokio::time::sleep(std::time::Duration::from_secs(wait_secs)) => {
            if cancel_token.is_cancelled() {
                Err(LLMError::Cancelled)
            } else {
                Ok(())
            }
        }
    }
}

pub(super) async fn next_stream_chunk(
    stream: &mut Pin<Box<dyn Stream<Item = Result<StreamChunk, LLMError>> + Send>>,
    cancel_token: &CancellationToken,
) -> Result<StreamChunk, LLMError> {
    tokio::select! {
        item = stream.next() => item.unwrap_or_else(|| Err(LLMError::Transport {
            kind: querymt::error::TransportErrorKind::ConnectionClosed,
            message: "LLM stream ended before a completion marker".to_string(),
        })),
        _ = cancel_token.cancelled() => Err(LLMError::Cancelled),
    }
}

pub(super) fn stream_chunk_commits_output(chunk: &StreamChunk) -> bool {
    match chunk {
        StreamChunk::Text(text) | StreamChunk::Thinking(text) => !text.is_empty(),
        StreamChunk::ThinkingSignature(_) => true,
        StreamChunk::ToolUseStart { .. } | StreamChunk::ToolUseComplete { .. } => true,
        StreamChunk::ToolUseInputDelta { partial_json, .. } => !partial_json.is_empty(),
        StreamChunk::Usage(_) | StreamChunk::Done { .. } => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StreamRetryDecision {
    pub(super) retry_ordinal: usize,
    pub(super) completed_attempt: usize,
    pub(super) semantic_output_seen: bool,
}

pub(super) struct StreamRetryBudget {
    retries_used: usize,
    max_stream_retries: usize,
    attempts_used: usize,
    max_attempts: usize,
}

impl StreamRetryBudget {
    pub(super) fn new(max_stream_retries: usize, max_attempts: usize) -> Self {
        Self {
            retries_used: 0,
            max_stream_retries,
            attempts_used: 0,
            max_attempts: max_attempts.max(1),
        }
    }

    pub(super) fn reserve_attempt(&mut self) -> Option<usize> {
        if self.attempts_used >= self.max_attempts {
            return None;
        }
        self.attempts_used += 1;
        Some(self.attempts_used)
    }

    pub(super) fn begin_retry(
        &mut self,
        error: &LLMError,
        semantic_output_seen: bool,
        cancelled: bool,
    ) -> Option<StreamRetryDecision> {
        if cancelled
            || !error.is_retryable()
            || self.retries_used >= self.max_stream_retries
            || self.attempts_used >= self.max_attempts
        {
            return None;
        }

        self.retries_used += 1;
        Some(StreamRetryDecision {
            retry_ordinal: self.retries_used,
            completed_attempt: self.attempts_used,
            semantic_output_seen,
        })
    }
}

#[derive(Debug)]
pub(super) enum StreamFailureAction {
    Retry,
    Cancelled,
    Terminal(LLMError),
}

pub(super) async fn handle_stream_failure(
    config: &AgentConfig,
    session_id: &str,
    error: LLMError,
    budget: &mut StreamRetryBudget,
    semantic_output_seen: bool,
    message_id: Option<String>,
    cancel_token: &CancellationToken,
) -> StreamFailureAction {
    if matches!(error, LLMError::Cancelled) || cancel_token.is_cancelled() {
        return StreamFailureAction::Cancelled;
    }

    let Some(retry) = budget.begin_retry(&error, semantic_output_seen, false) else {
        return StreamFailureAction::Terminal(error);
    };

    match wait_for_stream_retry(
        config,
        session_id,
        &error,
        retry,
        budget.max_stream_retries,
        message_id,
        cancel_token,
    )
    .await
    {
        Ok(()) => StreamFailureAction::Retry,
        Err(LLMError::Cancelled) => StreamFailureAction::Cancelled,
        Err(error) => StreamFailureAction::Terminal(error),
    }
}

pub(super) async fn wait_for_stream_retry(
    config: &AgentConfig,
    session_id: &str,
    error: &LLMError,
    retry: StreamRetryDecision,
    max_stream_retries: usize,
    message_id: Option<String>,
    cancel_token: &CancellationToken,
) -> Result<(), LLMError> {
    if retry.semantic_output_seen {
        // TODO(stream-retry-safety): replace already-emitted attempt deltas once
        // clients support attempt-scoped rollback/replacement.
        warn!(
            "Session {}: retrying stream after semantic output; the replacement request may duplicate visible output (request attempt {}/{}, stream recreation {}/{})",
            session_id,
            retry.completed_attempt,
            config.execution_policy.rate_limit.max_attempts(),
            retry.retry_ordinal,
            max_stream_retries,
        );
    }

    config.emit_event(
        session_id,
        AgentEventKind::StreamRecovering {
            message: error.to_string(),
            attempt: u32_from_usize(retry.retry_ordinal, "stream_retries_used", Some(session_id)),
            max_attempts: u32_from_usize(
                max_stream_retries,
                "max_stream_retries",
                Some(session_id),
            ),
            message_id,
        },
    );

    wait_for_retry(
        config,
        session_id,
        error,
        RetryWait {
            attempt: retry.completed_attempt,
            max_attempts: config.execution_policy.rate_limit.max_attempts(),
            wait_secs: retry_delay_secs(config, error, retry.completed_attempt),
        },
        cancel_token,
    )
    .await
}

// ══════════════════════════════════════════════════════════════════════════
//  Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_config::AgentConfig;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::core::ToolPolicy;
    use crate::config::RuntimeExecutionPolicy;
    use crate::events::AgentEventKind;
    use crate::test_utils::{
        MockLlmProvider, MockSessionStore, SharedLlmProvider, TestProviderFactory, mock_llm_config,
        mock_plugin_registry, mock_session,
    };
    use querymt::LLMParams;
    use querymt::chat::{ChatResponse, FinishReason, StreamChunk};
    use querymt::error::{LLMError, ProviderErrorKind, ProviderFailure};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::{Mutex, broadcast};
    use tokio_util::sync::CancellationToken;

    // ── Fixture ──────────────────────────────────────────────────────────────

    async fn make_config() -> (Arc<AgentConfig>, tempfile::TempDir) {
        let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
        let shared = SharedLlmProvider {
            inner: provider.clone(),
            tools: vec![].into_boxed_slice(),
        };
        let factory = Arc::new(TestProviderFactory::new(shared));
        let (plugin_registry, temp_dir) = mock_plugin_registry(factory).expect("plugin registry");

        let mut store = MockSessionStore::new();
        let llm_config = mock_llm_config();
        let session = mock_session("test-session");
        store
            .expect_get_session()
            .returning(move |_| Ok(Some(session.clone())))
            .times(0..);
        store
            .expect_get_session_llm_config()
            .returning(move |_| Ok(Some(llm_config.clone())))
            .times(0..);

        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .expect("create event store"),
        );

        let mut policy = RuntimeExecutionPolicy::default();
        policy.rate_limit.max_retries = 3;
        policy.rate_limit.default_wait_secs = 1;
        policy.rate_limit.backoff_multiplier = 2.0;

        let config = Arc::new(
            AgentConfigBuilder::new(
                Arc::new(plugin_registry),
                storage.clone(),
                LLMParams::new().provider("mock").model("mock-model"),
            )
            .with_tool_policy(ToolPolicy::ProviderOnly)
            .with_execution_policy(policy)
            .build(),
        );

        (config, temp_dir)
    }

    async fn recv_event(
        rx: &mut broadcast::Receiver<crate::events::EventEnvelope>,
    ) -> crate::events::EventEnvelope {
        tokio::time::timeout(tokio::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for retry event")
            .expect("retry event channel closed")
    }

    async fn assert_no_event(rx: &mut broadcast::Receiver<crate::events::EventEnvelope>) {
        assert!(
            tokio::time::timeout(tokio::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "unexpected retry event"
        );
    }

    // ── rate_limit_info (dual-form) tests ────────────────────────────────────

    #[test]
    fn test_rate_limit_info_rate_limited_with_retry_after() {
        let err = LLMError::RateLimited {
            message: "Too many requests".to_string(),
            retry_after_secs: Some(30),
        };
        let info = err.rate_limit_info();
        assert!(info.is_some());
        let (msg, retry_after) = info.unwrap();
        assert_eq!(msg, "Too many requests");
        assert_eq!(retry_after, Some(30));
    }

    #[test]
    fn test_rate_limit_info_rate_limited_no_retry_after() {
        let err = LLMError::RateLimited {
            message: "Quota exceeded".to_string(),
            retry_after_secs: None,
        };
        let info = err.rate_limit_info();
        assert!(info.is_some());
        let (msg, retry_after) = info.unwrap();
        assert_eq!(msg, "Quota exceeded");
        assert!(retry_after.is_none());
    }

    #[test]
    fn test_rate_limit_info_non_rate_limit_returns_none() {
        let err = LLMError::GenericError("something broke".to_string());
        assert!(err.rate_limit_info().is_none());

        let err = LLMError::HttpError("connection refused".to_string());
        assert!(err.rate_limit_info().is_none());
    }

    #[test]
    fn test_rate_limit_info_unified_rate_limited_kind() {
        let err = LLMError::RateLimited {
            message: "Rate limit reached".to_string(),
            retry_after_secs: Some(12),
        };
        let info = err.rate_limit_info().expect("RateLimited should extract");
        assert_eq!(info.0, "Rate limit reached");
        assert_eq!(info.1, Some(12));

        // Catch-all provider errors are not rate-limit UI events even if retryable.
        let non_rate = LLMError::from(
            ProviderFailure::new(ProviderErrorKind::UnknownTransient, "server broke")
                .with_code(Some("server_error".to_string()))
                .with_error_type(Some("api_error".to_string())),
        );
        assert!(non_rate.rate_limit_info().is_none());

        let overloaded = LLMError::from(
            ProviderFailure::new(ProviderErrorKind::ServerOverloaded, "busy")
                .with_code(Some("server_is_overloaded".into())),
        );
        assert!(overloaded.rate_limit_info().is_none());
        assert!(overloaded.is_retryable());
    }

    // ── retry delay tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_retry_delay_uses_retry_after() {
        let (config, _temp) = make_config().await;
        let error = LLMError::HttpStatus {
            status_code: 503,
            message: "unavailable".to_string(),
            retry_after_secs: Some(60),
        };
        assert_eq!(retry_delay_secs(&config, &error, 1), 60);
    }

    #[tokio::test]
    async fn test_retry_delay_uses_retry_ordinal_for_exponential_backoff() {
        let (config, _temp) = make_config().await;
        let error = LLMError::HttpStatus {
            status_code: 503,
            message: "unavailable".to_string(),
            retry_after_secs: None,
        };
        for (ordinal, expected) in [(1, 1), (2, 2), (3, 4)] {
            let actual = retry_delay_secs(&config, &error, ordinal);
            assert!(
                actual.abs_diff(expected) <= 1,
                "ordinal {ordinal}: expected jitter near {expected}, got {actual}"
            );
        }
    }

    #[test]
    fn test_calculated_jitter_is_bounded_and_deterministic() {
        assert_eq!(apply_jitter(100.0, 0.2, 0.0), 80);
        assert_eq!(apply_jitter(100.0, 0.2, 0.5), 100);
        assert_eq!(apply_jitter(100.0, 0.2, 1.0), 120);
    }

    #[tokio::test]
    async fn test_calculated_retry_delay_does_not_exceed_max_wait() {
        let (mut config, _temp) = make_config().await;
        let config = Arc::get_mut(&mut config).expect("config is uniquely owned");
        config.execution_policy.rate_limit.default_wait_secs = 1_000;
        config.execution_policy.rate_limit.max_wait_secs = 10;
        config.execution_policy.rate_limit.jitter_ratio = 1.0;

        let error = LLMError::HttpStatus {
            status_code: 503,
            message: "unavailable".to_string(),
            retry_after_secs: None,
        };
        assert!(retry_delay_secs(config, &error, 1) <= 10);
    }

    #[tokio::test]
    async fn test_provider_retry_hint_is_capped_by_max_wait() {
        let (mut config, _temp) = make_config().await;
        let config = Arc::get_mut(&mut config).expect("config is uniquely owned");
        config.execution_policy.rate_limit.max_wait_secs = 10;

        let error = LLMError::HttpStatus {
            status_code: 503,
            message: "unavailable".to_string(),
            retry_after_secs: Some(60),
        };
        assert_eq!(retry_delay_secs(config, &error, 1), 10);
    }

    // ── is_retryable tests ─────────────────────────────────────────────────

    #[test]
    fn test_is_retryable_transport() {
        let err = LLMError::Transport {
            kind: querymt::error::TransportErrorKind::ConnectionReset,
            message: "connection reset".to_string(),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn test_is_retryable_http_error_generic() {
        // HttpError (previously "error decoding response body" fell through here)
        let err = LLMError::HttpError("error decoding response body".to_string());
        assert!(err.is_retryable());
    }

    #[test]
    fn test_is_retryable_plugin_error() {
        let err = LLMError::PluginError("WASM runtime temporary failure".to_string());
        assert!(err.is_retryable());
    }

    #[test]
    fn deterministic_request_errors_are_not_retryable() {
        assert!(!LLMError::InvalidUrl("bad base url".into()).is_retryable());
        assert!(!LLMError::InvalidRequest("invalid header".into()).is_retryable());
        assert!(
            !LLMError::ResponseFormatError {
                message: "invalid json".into(),
                raw_response: "not json".into(),
            }
            .is_retryable()
        );
    }

    #[test]
    fn test_is_retryable_transport_body() {
        // The new typed path for reqwest is_body() errors
        let err = LLMError::Transport {
            kind: querymt::error::TransportErrorKind::Other,
            message: "error decoding response body".to_string(),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn test_is_retryable_http_status_503() {
        let err = LLMError::HttpStatus {
            status_code: 503,
            message: "upstream unavailable".to_string(),
            retry_after_secs: None,
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn test_is_retryable_non_transient() {
        assert!(!LLMError::GenericError("oops".to_string()).is_retryable());
    }

    // ── cancellation-aware wait tests ────────────────────────────────────────

    #[tokio::test]
    async fn test_wait_for_retry_delay_completes_normally() {
        let token = CancellationToken::new();
        assert!(wait_for_retry_delay(0, &token).await.is_ok());
    }

    #[tokio::test]
    async fn test_wait_for_retry_delay_cancelled_early() {
        let token = CancellationToken::new();
        token.cancel();
        assert!(matches!(
            wait_for_retry_delay(60, &token).await,
            Err(LLMError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn test_wait_for_retry_delay_cancelled_during_wait() {
        let token = CancellationToken::new();
        let token_clone = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            token_clone.cancel();
        });
        assert!(matches!(
            wait_for_retry_delay(60, &token).await,
            Err(LLMError::Cancelled)
        ));
    }

    // ── call_with_retry tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_call_with_retry_succeeds_first_attempt() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();

        let result = call_with_retry(&config, "test-session", &token, || {
            let count = call_count2.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                let resp: Box<dyn ChatResponse> =
                    Box::new(crate::test_utils::MockChatResponse::text_only("hello"));
                Ok(resp)
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn generic_call_with_retry_supports_provider_initialization() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();
        let provider: Arc<dyn querymt::LLMProvider> = Arc::new(MockLlmProvider::new());

        let result = call_with_retry(&config, "test-session", &token, || {
            let count = call_count2.clone();
            let provider = Arc::clone(&provider);
            async move {
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(LLMError::Transport {
                        kind: querymt::error::TransportErrorKind::ConnectionRefused,
                        message: "registry temporarily unavailable".into(),
                    })
                } else {
                    Ok(provider)
                }
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_call_with_retry_fails_non_rate_limit() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();

        let result = call_with_retry(&config, "test-session", &token, || {
            let count = call_count2.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Err::<Box<dyn ChatResponse>, _>(LLMError::GenericError("fatal error".to_string()))
            }
        })
        .await;

        // Non-rate-limit errors should fail immediately without retrying
        assert!(result.is_err());
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_call_with_retry_retries_transient_setup_errors() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();

        let result = call_with_retry(&config, "test-session", &token, || {
            let count = call_count2.clone();
            async move {
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    Err::<Box<dyn ChatResponse>, _>(LLMError::HttpStatus {
                        status_code: 503,
                        message: "upstream connect error".to_string(),
                        retry_after_secs: Some(0),
                    })
                } else {
                    Ok::<Box<dyn ChatResponse>, _>(Box::new(
                        crate::test_utils::MockChatResponse::text_only("ok"),
                    ) as Box<dyn ChatResponse>)
                }
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_call_with_retry_cancelled_before_start() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        token.cancel();

        let result = call_with_retry(&config, "test-session", &token, || async {
            Ok::<Box<dyn ChatResponse>, _>(Box::new(crate::test_utils::MockChatResponse::text_only(
                "should not get here",
            )) as Box<dyn ChatResponse>)
        })
        .await;

        assert!(matches!(result, Err(LLMError::Cancelled)));
    }

    #[tokio::test]
    async fn test_call_with_retry_cancellation_during_delay_starts_no_retry() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        let token_clone = token.clone();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();

        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            token_clone.cancel();
        });
        let result = call_with_retry(&config, "test-session", &token, || {
            let count = call_count2.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Err::<Box<dyn ChatResponse>, _>(LLMError::HttpStatus {
                    status_code: 503,
                    message: "unavailable".to_string(),
                    retry_after_secs: Some(60),
                })
            }
        })
        .await;

        assert!(matches!(result, Err(LLMError::Cancelled)));
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rate_limit_wait_event_uses_clamped_provider_hint() {
        let (mut config, _temp) = make_config().await;
        Arc::get_mut(&mut config)
            .expect("config is uniquely owned")
            .execution_policy
            .rate_limit
            .max_wait_secs = 0;
        let mut events = config.subscribe_events();
        let token = CancellationToken::new();
        let attempt_count = Arc::new(AtomicUsize::new(0));
        let attempt_count_for_call = Arc::clone(&attempt_count);

        let result = call_with_retry(&config, "test-session", &token, || {
            let attempt_count = Arc::clone(&attempt_count_for_call);
            async move {
                if attempt_count.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err::<Box<dyn ChatResponse>, _>(LLMError::RateLimited {
                        message: "rate limited".into(),
                        retry_after_secs: Some(60),
                    })
                } else {
                    Ok::<Box<dyn ChatResponse>, _>(Box::new(
                        crate::test_utils::MockChatResponse::text_only("ok"),
                    ) as Box<dyn ChatResponse>)
                }
            }
        })
        .await;

        assert!(result.is_ok());
        let wait = recv_event(&mut events).await;
        assert!(wait.is_durable());
        assert!(matches!(
            wait.kind(),
            AgentEventKind::RateLimited { wait_secs: 0, .. }
        ));
        let resume = recv_event(&mut events).await;
        assert!(resume.is_durable());
        assert!(matches!(
            resume.kind(),
            AgentEventKind::RateLimitResume { attempt: 2 }
        ));
        assert!(
            resume.seq() > wait.seq(),
            "resume must be persisted after wait"
        );
    }

    #[tokio::test]
    async fn test_retryable_overload_emits_wait_and_resume_events_in_order() {
        let (config, _temp) = make_config().await;
        let mut events = config.subscribe_events();
        let token = CancellationToken::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();

        let result = call_with_retry(&config, "test-session", &token, || {
            let count = call_count2.clone();
            async move {
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err::<Box<dyn ChatResponse>, _>(LLMError::from(
                        ProviderFailure::new(ProviderErrorKind::ServerOverloaded, "busy")
                            .with_retry_after_secs(Some(0)),
                    ))
                } else {
                    Ok::<Box<dyn ChatResponse>, _>(Box::new(
                        crate::test_utils::MockChatResponse::text_only("ok"),
                    ) as Box<dyn ChatResponse>)
                }
            }
        })
        .await;

        assert!(result.is_ok());
        let wait = recv_event(&mut events).await;
        assert!(wait.is_durable());
        assert!(matches!(
            wait.kind(),
            AgentEventKind::LlmRetryWait {
                wait_secs: 0,
                attempt: 1,
                max_attempts: 3,
                ..
            }
        ));
        let resume = recv_event(&mut events).await;
        assert!(resume.is_durable());
        assert!(matches!(
            resume.kind(),
            AgentEventKind::LlmRetryResume { attempt: 2 }
        ));
        assert!(
            resume.seq() > wait.seq(),
            "resume must be persisted after wait"
        );
        assert_no_event(&mut events).await;
    }

    #[tokio::test]
    async fn permanent_error_emits_no_retry_events() {
        let (config, _temp) = make_config().await;
        let mut events = config.subscribe_events();
        let token = CancellationToken::new();

        let result = call_with_retry(&config, "test-session", &token, || async {
            Err::<Box<dyn ChatResponse>, _>(LLMError::AuthError("bad key".into()))
        })
        .await;

        assert!(matches!(result, Err(LLMError::AuthError(_))));
        assert_no_event(&mut events).await;
    }

    #[tokio::test]
    async fn cancellation_during_generic_retry_wait_emits_no_resume_event() {
        let (config, _temp) = make_config().await;
        let mut events = config.subscribe_events();
        let token = CancellationToken::new();
        let token_clone = token.clone();

        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            token_clone.cancel();
        });
        let result = call_with_retry(&config, "test-session", &token, || async {
            Err::<Box<dyn ChatResponse>, _>(LLMError::from(
                ProviderFailure::new(ProviderErrorKind::ServerOverloaded, "busy")
                    .with_retry_after_secs(Some(60)),
            ))
        })
        .await;

        assert!(matches!(result, Err(LLMError::Cancelled)));
        let wait = recv_event(&mut events).await;
        assert!(matches!(wait.kind(), AgentEventKind::LlmRetryWait { .. }));
        assert_no_event(&mut events).await;
    }

    #[tokio::test]
    async fn test_call_with_retry_rate_limit_exhausted() {
        let (config, _temp) = make_config().await;
        // max_retries = 3
        let token = CancellationToken::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();

        let result = call_with_retry(&config, "test-session", &token, || {
            let count = call_count2.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Err::<Box<dyn ChatResponse>, _>(LLMError::RateLimited {
                    message: "rate limited".to_string(),
                    retry_after_secs: Some(0), // 0s wait to keep test fast
                })
            }
        })
        .await;

        assert!(matches!(
            result,
            Err(LLMError::RateLimited {
                message,
                retry_after_secs: Some(0),
            }) if message == "rate limited"
        ));
        // max_retries is the total number of setup attempts.
        assert_eq!(call_count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn stream_budget_reserves_only_configured_attempts() {
        let mut budget = StreamRetryBudget::new(2, 3);

        assert_eq!(budget.reserve_attempt(), Some(1));
        assert_eq!(budget.reserve_attempt(), Some(2));
        assert_eq!(budget.reserve_attempt(), Some(3));
        assert_eq!(budget.reserve_attempt(), None);
    }

    #[tokio::test]
    async fn test_call_with_retry_permanent_quota_is_called_exactly_once() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();

        let result = call_with_retry(&config, "test-session", &token, || {
            let count = call_count2.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Err::<Box<dyn ChatResponse>, _>(LLMError::from(
                    ProviderFailure::new(
                        ProviderErrorKind::QuotaExceeded,
                        "You have hit your usage limit.",
                    )
                    .with_code(Some("usage_limit_reached".into()))
                    .with_error_type(Some("usage_limit_reached".into())),
                ))
            }
        })
        .await;

        assert!(matches!(
            result,
            Err(LLMError::ProviderResponseError(failure))
                if failure.kind() == querymt::error::ProviderErrorKind::QuotaExceeded
        ));
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "permanent quota must not be retried"
        );
    }

    #[tokio::test]
    async fn test_call_with_retry_auth_error_is_called_exactly_once() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();

        let result = call_with_retry(&config, "test-session", &token, || {
            let count = call_count2.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Err::<Box<dyn ChatResponse>, _>(LLMError::AuthError("bad key".into()))
            }
        })
        .await;

        assert!(matches!(result, Err(LLMError::AuthError(_))));
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exhausted_stream_budget_returns_the_original_typed_error() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        let mut budget = StreamRetryBudget::new(2, 1);
        assert_eq!(budget.reserve_attempt(), Some(1));
        assert_eq!(budget.reserve_attempt(), None);
        let error = overloaded_error(Some(7));

        let action = handle_stream_failure(
            &config,
            "test-session",
            error,
            &mut budget,
            false,
            None,
            &token,
        )
        .await;

        assert!(matches!(
            action,
            StreamFailureAction::Terminal(LLMError::ProviderResponseError(failure))
                if failure.kind() == ProviderErrorKind::ServerOverloaded
                    && failure.code() == Some("server_is_overloaded")
                    && failure.request_id() == Some("request-3")
                    && failure.retry_after_secs() == Some(7)
        ));
    }

    #[tokio::test]
    async fn stream_setup_and_parser_failures_share_one_physical_request_budget() {
        let (mut config, _temp) = make_config().await;
        let config_mut = Arc::get_mut(&mut config).expect("config uniquely owned");
        config_mut.execution_policy.rate_limit.default_wait_secs = 0;
        let token = CancellationToken::new();
        let mut budget = StreamRetryBudget::new(2, 3);

        assert_eq!(budget.reserve_attempt(), Some(1));
        let setup_error = LLMError::HttpStatus {
            status_code: 503,
            message: "setup failed".into(),
            retry_after_secs: Some(0),
        };
        assert!(matches!(
            handle_stream_failure(
                &config,
                "test-session",
                setup_error,
                &mut budget,
                false,
                None,
                &token,
            )
            .await,
            StreamFailureAction::Retry
        ));

        assert_eq!(budget.reserve_attempt(), Some(2));
        let parser_error = overloaded_error(Some(0));
        assert!(matches!(
            handle_stream_failure(
                &config,
                "test-session",
                parser_error,
                &mut budget,
                true,
                None,
                &token,
            )
            .await,
            StreamFailureAction::Retry
        ));

        assert_eq!(budget.reserve_attempt(), Some(3));
        assert_eq!(budget.reserve_attempt(), None);
    }

    #[tokio::test]
    async fn two_parser_overloads_can_use_the_full_shared_request_budget() {
        let (mut config, _temp) = make_config().await;
        let config_mut = Arc::get_mut(&mut config).expect("config uniquely owned");
        config_mut.execution_policy.rate_limit.default_wait_secs = 0;
        let token = CancellationToken::new();
        let mut budget = StreamRetryBudget::new(2, 3);

        for attempt in 1..=3 {
            assert_eq!(budget.reserve_attempt(), Some(attempt));
            if attempt < 3 {
                assert!(matches!(
                    handle_stream_failure(
                        &config,
                        "test-session",
                        overloaded_error(Some(0)),
                        &mut budget,
                        false,
                        None,
                        &token,
                    )
                    .await,
                    StreamFailureAction::Retry
                ));
            }
        }

        assert_eq!(budget.reserve_attempt(), None);
        assert_eq!(budget.retries_used, 2);
    }

    #[test]
    fn explicit_stream_retry_cap_still_limits_recreations() {
        let error = overloaded_error(Some(0));
        let mut budget = StreamRetryBudget::new(1, 3);
        assert_eq!(budget.reserve_attempt(), Some(1));

        assert!(budget.begin_retry(&error, false, false).is_some());
        assert_eq!(budget.reserve_attempt(), Some(2));
        assert_eq!(budget.begin_retry(&error, false, false), None);
    }

    #[test]
    fn malformed_terminal_can_consume_the_same_stream_retry_budget() {
        let error = LLMError::from(
            ProviderFailure::new(
                ProviderErrorKind::UnknownTransient,
                "tool_calls without completed calls",
            )
            .with_code(Some("empty_tool_calls_terminal".into())),
        );
        let mut budget = StreamRetryBudget::new(2, 3);
        assert_eq!(budget.reserve_attempt(), Some(1));

        assert!(budget.begin_retry(&error, true, false).is_some());
        assert_eq!(budget.retries_used, 1);
    }

    #[test]
    fn any_stage_stream_retry_is_allowed_but_reported_as_post_output() {
        let error = overloaded_error(Some(0));
        let mut budget = StreamRetryBudget::new(2, 3);
        assert_eq!(budget.reserve_attempt(), Some(1));

        let retry = budget
            .begin_retry(&error, true, false)
            .expect("main-compatible retries remain allowed after output");
        assert_eq!(retry.retry_ordinal, 1);
        assert_eq!(retry.completed_attempt, 1);
        assert!(retry.semantic_output_seen);
    }

    #[tokio::test]
    async fn parser_retry_uses_ordered_wait_and_resume_events() {
        let (mut config, _temp) = make_config().await;
        let config_mut = Arc::get_mut(&mut config).expect("config uniquely owned");
        config_mut.execution_policy.rate_limit.default_wait_secs = 0;
        config_mut.execution_policy.rate_limit.max_stream_retries = 2;
        let mut events = config.subscribe_events();
        let token = CancellationToken::new();
        let mut budget = StreamRetryBudget::new(2, 3);
        assert_eq!(budget.reserve_attempt(), Some(1));

        let action = handle_stream_failure(
            &config,
            "test-session",
            overloaded_error(Some(0)),
            &mut budget,
            true,
            None,
            &token,
        )
        .await;
        assert!(matches!(action, StreamFailureAction::Retry));

        let recovering = recv_event(&mut events).await;
        assert!(matches!(
            recovering.kind(),
            AgentEventKind::StreamRecovering {
                attempt: 1,
                max_attempts: 2,
                ..
            }
        ));
        let wait = recv_event(&mut events).await;
        assert!(matches!(
            wait.kind(),
            AgentEventKind::LlmRetryWait {
                attempt: 1,
                max_attempts: 3,
                ..
            }
        ));
        let resume = recv_event(&mut events).await;
        assert!(matches!(
            resume.kind(),
            AgentEventKind::LlmRetryResume { attempt: 2 }
        ));
        assert!(resume.seq() > wait.seq());
    }

    #[tokio::test]
    async fn next_stream_chunk_turns_unexpected_eof_into_retryable_error() {
        let mut empty: Pin<Box<dyn Stream<Item = Result<StreamChunk, LLMError>> + Send>> =
            Box::pin(futures_util::stream::empty());
        let error = next_stream_chunk(&mut empty, &CancellationToken::new())
            .await
            .expect_err("missing Done must be an error");

        assert!(matches!(
            error,
            LLMError::Transport {
                kind: querymt::error::TransportErrorKind::ConnectionClosed,
                ..
            }
        ));
        assert!(error.is_retryable());
    }

    #[test]
    fn stream_chunk_commit_policy_tracks_main_compatibility_warning_boundary() {
        assert!(!stream_chunk_commits_output(&StreamChunk::Text(
            String::new()
        )));
        assert!(stream_chunk_commits_output(&StreamChunk::Text("x".into())));
        assert!(stream_chunk_commits_output(&StreamChunk::Thinking(
            "x".into()
        )));
        assert!(stream_chunk_commits_output(
            &StreamChunk::ThinkingSignature(String::new())
        ));
        assert!(stream_chunk_commits_output(&StreamChunk::ToolUseStart {
            index: 0,
            id: "call-1".into(),
            name: "read".into(),
        }));
        assert!(stream_chunk_commits_output(
            &StreamChunk::ToolUseInputDelta {
                index: 0,
                partial_json: "{".into(),
            }
        ));
        assert!(!stream_chunk_commits_output(&StreamChunk::Done {
            finish_reason: FinishReason::Stop,
        }));
    }

    fn overloaded_error(retry_after_secs: Option<u64>) -> LLMError {
        LLMError::from(
            ProviderFailure::new(ProviderErrorKind::ServerOverloaded, "overloaded")
                .with_code(Some("server_is_overloaded".into()))
                .with_request_id(Some("request-3".into()))
                .with_retry_after_secs(retry_after_secs),
        )
    }
}
