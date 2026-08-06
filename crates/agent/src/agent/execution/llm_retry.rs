//! LLM retry logic with rate limit handling
//!
//! This module handles retrying LLM calls with exponential backoff when rate limits are hit.

use crate::agent::agent_config::AgentConfig;
use crate::agent::utils::u32_from_usize;
use crate::events::AgentEventKind;
use futures_util::{Stream, StreamExt};
use log::{debug, info};
use querymt::chat::StreamChunk;
use querymt::error::LLMError;
use std::future::Future;
use std::pin::Pin;
use tokio_util::sync::CancellationToken;
use tracing::{Span, instrument};

/// Call an LLM with automatic retry on rate limit errors.
///
/// This function wraps any LLM call and automatically retries with exponential backoff
/// when rate limit errors are detected. It respects cancellation via the `cancel_rx` channel.
#[instrument(
    name = "agent.llm.call_with_retry",
    skip(config, cancel_token, call_fn),
    fields(session_id = %session_id, attempt = tracing::field::Empty, rate_limited = tracing::field::Empty)
)]
pub(super) async fn call_llm_with_retry<F, Fut>(
    config: &AgentConfig,
    session_id: &str,
    cancel_token: &CancellationToken,
    mut call_fn: F,
) -> Result<Box<dyn querymt::chat::ChatResponse>, LLMError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Box<dyn querymt::chat::ChatResponse>, LLMError>>,
{
    let max_attempts = config.execution_policy.rate_limit.max_attempts();
    let mut attempt = 0;
    Span::current().record("rate_limited", false);

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
            Err(e) => {
                Span::current().record("attempt", attempt);
                let rate_limit_info = e.rate_limit_info();
                Span::current().record("rate_limited", rate_limit_info.is_some());

                if !e.is_retryable() || attempt >= max_attempts {
                    return Err(e);
                }

                let wait_secs = retry_delay_secs(config, &e, attempt);
                let is_rate_limited = rate_limit_info.is_some();
                if let Some((message, _)) = rate_limit_info {
                    let started_at = time::OffsetDateTime::now_utc().unix_timestamp();
                    info!(
                        "Session {} rate limited, attempt {}/{}, waiting {}s",
                        session_id, attempt, max_attempts, wait_secs
                    );
                    config.emit_event(
                        session_id,
                        AgentEventKind::RateLimited {
                            message,
                            wait_secs,
                            started_at,
                            attempt: u32_from_usize(attempt, "attempt", Some(session_id)),
                            max_attempts: u32_from_usize(
                                max_attempts,
                                "max_attempts",
                                Some(session_id),
                            ),
                        },
                    );
                } else {
                    debug!(
                        "Session {} transient setup error on attempt {}, retrying in {}s: {}",
                        session_id, attempt, wait_secs, e
                    );
                }

                wait_for_retry_delay(wait_secs, cancel_token).await?;

                if is_rate_limited {
                    info!(
                        "Session {} resuming after rate limit wait, attempt {}",
                        session_id,
                        attempt + 1
                    );
                    config.emit_event(
                        session_id,
                        AgentEventKind::RateLimitResume {
                            attempt: u32_from_usize(
                                attempt.saturating_add(1),
                                "attempt + 1",
                                Some(session_id),
                            ),
                        },
                    );
                }
            }
        }
    }
}

/// Calculate the delay for a retry, preferring the provider's retry hint.
pub(super) fn retry_delay_secs(
    config: &AgentConfig,
    error: &LLMError,
    retry_ordinal: usize,
) -> u64 {
    if let Some(secs) = error.retry_after_secs() {
        return secs;
    }

    let policy = &config.execution_policy.rate_limit;
    let base = policy.default_wait_secs as f64;
    let multiplier = policy.backoff_multiplier;
    let calculated = base * multiplier.powi(retry_ordinal.saturating_sub(1) as i32);
    apply_jitter(calculated, policy.jitter_ratio, jitter_sample()).min(policy.max_wait_secs)
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
#[instrument(name = "agent.llm.rate_limit_wait", skip(cancel_token), fields(wait_secs = wait_secs))]
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

/// Create a streaming connection with retry logic for rate limits and transient errors.
///
/// This retries stream creation only. Mid-stream recovery is handled by the consumer,
/// which may recreate a stream only before it observes semantic output.
pub(super) async fn create_stream_with_retry<F, Fut>(
    config: &AgentConfig,
    session_id: &str,
    cancel_token: &CancellationToken,
    attempts_used: &mut usize,
    create_stream: F,
) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, LLMError>> + Send>>, LLMError>
where
    F: Fn() -> Fut,
    Fut: Future<
        Output = Result<
            Pin<Box<dyn Stream<Item = Result<StreamChunk, LLMError>> + Send>>,
            LLMError,
        >,
    >,
{
    let max_attempts = config.execution_policy.rate_limit.max_attempts();

    loop {
        if cancel_token.is_cancelled() {
            return Err(LLMError::Cancelled);
        }

        // Shared request budget is enforced here, not via debug_assert.
        // Callers should also refuse re-entry via StreamRetryBudget, but a
        // broken invariant must still return a real error in release builds.
        // GenericError is intentionally non-retryable — a blown internal
        // budget must not look like transient transport failure.
        if *attempts_used >= max_attempts {
            return Err(LLMError::GenericError(format!(
                "stream request attempt budget exhausted ({attempts_used}/{max_attempts})"
            )));
        }
        *attempts_used += 1;
        let attempt = *attempts_used;

        match create_stream().await {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                let rate_limit_info = e.rate_limit_info();
                if !e.is_retryable() || attempt >= max_attempts {
                    return Err(e);
                }

                let wait_secs = retry_delay_secs(config, &e, attempt);
                let is_rate_limited = rate_limit_info.is_some();
                if let Some((message, _)) = rate_limit_info {
                    let started_at = time::OffsetDateTime::now_utc().unix_timestamp();
                    info!(
                        "Session {} rate limited (streaming), attempt {}/{}, waiting {}s",
                        session_id, attempt, max_attempts, wait_secs
                    );
                    config.emit_event(
                        session_id,
                        AgentEventKind::RateLimited {
                            message,
                            wait_secs,
                            started_at,
                            attempt: u32_from_usize(attempt, "attempt", Some(session_id)),
                            max_attempts: u32_from_usize(
                                max_attempts,
                                "max_attempts",
                                Some(session_id),
                            ),
                        },
                    );
                } else {
                    debug!(
                        "Session {} transient stream setup error on attempt {}, retrying in {}s: {}",
                        session_id, attempt, wait_secs, e
                    );
                }

                wait_for_retry_delay(wait_secs, cancel_token).await?;

                if is_rate_limited {
                    config.emit_event(
                        session_id,
                        AgentEventKind::RateLimitResume {
                            attempt: u32_from_usize(
                                attempt.saturating_add(1),
                                "attempt + 1",
                                Some(session_id),
                            ),
                        },
                    );
                }
            }
        }
    }
}

/// Read the next chunk, mapping stream EOF and cancellation to typed errors.
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

pub(super) struct StreamRetryBudget {
    retries_used: usize,
    max_retries: usize,
    attempts_used: usize,
    max_attempts: usize,
}

impl StreamRetryBudget {
    pub(super) fn new(max_retries: usize, max_attempts: usize) -> Self {
        Self {
            retries_used: 0,
            max_retries,
            attempts_used: 0,
            max_attempts: max_attempts.max(1),
        }
    }

    pub(super) fn attempts_used_mut(&mut self) -> &mut usize {
        &mut self.attempts_used
    }

    pub(super) fn begin_retry(
        &mut self,
        error: &LLMError,
        semantic_output_seen: bool,
        cancelled: bool,
    ) -> Option<usize> {
        if cancelled
            || semantic_output_seen
            || !error.is_retryable()
            || self.retries_used >= self.max_retries
            || self.attempts_used >= self.max_attempts
        {
            return None;
        }

        self.retries_used += 1;
        Some(self.retries_used)
    }
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
    use crate::test_utils::{
        MockLlmProvider, MockSessionStore, SharedLlmProvider, TestProviderFactory, mock_llm_config,
        mock_plugin_registry, mock_session,
    };
    use querymt::LLMParams;
    use querymt::chat::{ChatResponse, FinishReason, StreamChunk};
    use querymt::error::{LLMError, ProviderErrorContext, ProviderErrorKind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;
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
        let non_rate = LLMError::ProviderResponseError {
            message: "server broke".to_string(),
            context: Box::new(ProviderErrorContext {
                provider: "openai".to_string(),
                kind: ProviderErrorKind::UnknownTransient,
                code: Some("server_error".to_string()),
                error_type: Some("api_error".to_string()),
                request_id: None,
                retry_after_secs: None,
            }),
        };
        assert!(non_rate.rate_limit_info().is_none());

        let overloaded = LLMError::ProviderResponseError {
            message: "busy".into(),
            context: Box::new(ProviderErrorContext {
                provider: "codex".into(),
                kind: querymt::error::ProviderErrorKind::ServerOverloaded,
                code: Some("server_is_overloaded".into()),
                error_type: None,
                request_id: None,
                retry_after_secs: None,
            }),
        };
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
    async fn test_provider_retry_hint_remains_authoritative_above_max_wait() {
        let (mut config, _temp) = make_config().await;
        let config = Arc::get_mut(&mut config).expect("config is uniquely owned");
        config.execution_policy.rate_limit.max_wait_secs = 10;

        let error = LLMError::HttpStatus {
            status_code: 503,
            message: "unavailable".to_string(),
            retry_after_secs: Some(60),
        };
        assert_eq!(retry_delay_secs(config, &error, 1), 60);
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
        let err = LLMError::PluginError("wasm stream failed".to_string());
        assert!(err.is_retryable());
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

    // ── call_llm_with_retry tests ────────────────────────────────────────────

    #[tokio::test]
    async fn test_call_llm_with_retry_succeeds_first_attempt() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();

        let result = call_llm_with_retry(&config, "test-session", &token, || {
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
    async fn test_call_llm_with_retry_fails_non_rate_limit() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();

        let result = call_llm_with_retry(&config, "test-session", &token, || {
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
    async fn test_call_llm_with_retry_retries_transient_setup_errors() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();

        let result = call_llm_with_retry(&config, "test-session", &token, || {
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
    async fn test_call_llm_with_retry_cancelled_before_start() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        token.cancel();

        let result = call_llm_with_retry(&config, "test-session", &token, || async {
            Ok::<Box<dyn ChatResponse>, _>(Box::new(crate::test_utils::MockChatResponse::text_only(
                "should not get here",
            )) as Box<dyn ChatResponse>)
        })
        .await;

        assert!(matches!(result, Err(LLMError::Cancelled)));
    }

    #[tokio::test]
    async fn test_call_llm_with_retry_cancellation_during_delay_starts_no_retry() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        let token_clone = token.clone();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();

        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            token_clone.cancel();
        });
        let result = call_llm_with_retry(&config, "test-session", &token, || {
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
    async fn test_call_llm_with_retry_rate_limit_exhausted() {
        let (config, _temp) = make_config().await;
        // max_retries = 3
        let token = CancellationToken::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();

        let result = call_llm_with_retry(&config, "test-session", &token, || {
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

    #[tokio::test]
    async fn test_create_stream_with_retry_preserves_typed_provider_error_on_exhaustion() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();
        let mut attempts_used = 0;

        let result =
            create_stream_with_retry(&config, "test-session", &token, &mut attempts_used, || {
                let count = call_count2.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Err(LLMError::ProviderResponseError {
                        message: "overloaded".into(),
                        context: Box::new(ProviderErrorContext {
                            provider: "test".into(),
                            kind: querymt::error::ProviderErrorKind::ServerOverloaded,
                            code: Some("server_is_overloaded".into()),
                            error_type: None,
                            request_id: Some("request-3".into()),
                            retry_after_secs: Some(0),
                        }),
                    })
                }
            })
            .await;

        assert!(matches!(
            result,
            Err(LLMError::ProviderResponseError { message, context })
                if message == "overloaded"
                    && context.kind == querymt::error::ProviderErrorKind::ServerOverloaded
                    && context.request_id.as_deref() == Some("request-3")
        ));
        assert_eq!(call_count.load(Ordering::SeqCst), 3);
        assert_eq!(attempts_used, 3);
    }

    #[tokio::test]
    async fn test_create_stream_with_retry_hard_errors_when_budget_already_exhausted() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();
        // max_attempts defaults to 3; start already exhausted.
        let mut attempts_used = config.execution_policy.rate_limit.max_attempts();

        let result =
            create_stream_with_retry(&config, "test-session", &token, &mut attempts_used, || {
                let count = call_count2.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Err(LLMError::HttpStatus {
                        status_code: 503,
                        message: "should not be called".into(),
                        retry_after_secs: None,
                    })
                }
            })
            .await;

        let err = result.err().expect("budget exhaust must error");
        match &err {
            LLMError::GenericError(message) => {
                assert!(message.contains("attempt budget exhausted"));
            }
            other => panic!("expected GenericError, got {other}"),
        }
        assert!(!err.is_retryable());
        assert_eq!(call_count.load(Ordering::SeqCst), 0);
        assert_eq!(
            attempts_used,
            config.execution_policy.rate_limit.max_attempts()
        );
    }

    #[tokio::test]
    async fn test_call_llm_with_retry_permanent_quota_is_called_exactly_once() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();

        let result = call_llm_with_retry(&config, "test-session", &token, || {
            let count = call_count2.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Err::<Box<dyn ChatResponse>, _>(LLMError::ProviderResponseError {
                    message: "You have hit your usage limit.".into(),
                    context: Box::new(ProviderErrorContext {
                        provider: "codex".into(),
                        kind: querymt::error::ProviderErrorKind::QuotaExceeded,
                        code: Some("usage_limit_reached".into()),
                        error_type: Some("usage_limit_reached".into()),
                        request_id: None,
                        retry_after_secs: None,
                    }),
                })
            }
        })
        .await;

        assert!(matches!(
            result,
            Err(LLMError::ProviderResponseError { context, .. })
                if context.kind == querymt::error::ProviderErrorKind::QuotaExceeded
        ));
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "permanent quota must not be retried"
        );
    }

    #[tokio::test]
    async fn test_call_llm_with_retry_auth_error_is_called_exactly_once() {
        let (config, _temp) = make_config().await;
        let token = CancellationToken::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = call_count.clone();

        let result = call_llm_with_retry(&config, "test-session", &token, || {
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
    async fn test_next_stream_chunk_maps_eof_and_cancellation() {
        let mut empty: Pin<Box<dyn Stream<Item = Result<StreamChunk, LLMError>> + Send>> =
            Box::pin(futures_util::stream::empty());
        let error = next_stream_chunk(&mut empty, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            LLMError::Transport {
                kind: querymt::error::TransportErrorKind::ConnectionClosed,
                ..
            }
        ));

        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut pending: Pin<Box<dyn Stream<Item = Result<StreamChunk, LLMError>> + Send>> =
            Box::pin(futures_util::stream::pending());
        assert!(matches!(
            next_stream_chunk(&mut pending, &cancel).await,
            Err(LLMError::Cancelled)
        ));
    }

    #[test]
    fn test_stream_chunk_commit_policy_covers_semantic_variants() {
        assert!(!stream_chunk_commits_output(&StreamChunk::Text(
            String::new()
        )));
        assert!(stream_chunk_commits_output(&StreamChunk::Text("x".into())));
        assert!(!stream_chunk_commits_output(&StreamChunk::Thinking(
            String::new()
        )));
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
        assert!(!stream_chunk_commits_output(
            &StreamChunk::ToolUseInputDelta {
                index: 0,
                partial_json: String::new(),
            }
        ));
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

    #[test]
    fn test_thinking_and_tool_output_prevent_stream_retry() {
        let error = LLMError::HttpStatus {
            status_code: 503,
            message: "unavailable".into(),
            retry_after_secs: None,
        };
        for chunk in [
            StreamChunk::Thinking("reasoning".into()),
            StreamChunk::ThinkingSignature("signature".into()),
            StreamChunk::ToolUseStart {
                index: 0,
                id: "call-1".into(),
                name: "read".into(),
            },
            StreamChunk::ToolUseInputDelta {
                index: 0,
                partial_json: "{".into(),
            },
        ] {
            let mut budget = StreamRetryBudget::new(1, 3);
            *budget.attempts_used_mut() = 1;
            assert_eq!(
                budget.begin_retry(&error, stream_chunk_commits_output(&chunk), false),
                None,
            );
            assert_eq!(budget.retries_used, 0);
        }
    }

    #[test]
    fn test_stream_retry_policy_stops_when_shared_request_budget_is_exhausted() {
        let error = LLMError::HttpStatus {
            status_code: 503,
            message: "unavailable".into(),
            retry_after_secs: None,
        };
        let mut budget = StreamRetryBudget::new(5, 3);
        *budget.attempts_used_mut() = 3;

        assert_eq!(budget.begin_retry(&error, false, false), None);
        assert_eq!(budget.retries_used, 0);
    }

    #[test]
    fn test_stream_retry_policy_counts_retries_and_stops_after_output_or_exhaustion() {
        let error = LLMError::HttpStatus {
            status_code: 503,
            message: "unavailable".into(),
            retry_after_secs: None,
        };
        let mut budget = StreamRetryBudget::new(1, 3);
        *budget.attempts_used_mut() = 1;

        assert_eq!(budget.begin_retry(&error, false, false), Some(1));
        assert_eq!(budget.retries_used, 1);
        assert_eq!(budget.begin_retry(&error, false, false), None);

        let mut output_budget = StreamRetryBudget::new(2, 3);
        *output_budget.attempts_used_mut() = 1;
        assert_eq!(output_budget.begin_retry(&error, true, false), None);
        assert_eq!(output_budget.begin_retry(&error, false, true), None);
    }
}
