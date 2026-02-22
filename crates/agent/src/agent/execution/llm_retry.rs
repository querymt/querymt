//! LLM retry logic with rate limit handling
//!
//! This module handles retrying LLM calls with exponential backoff when rate limits are hit.

use crate::agent::agent_config::AgentConfig;
use crate::events::AgentEventKind;
use futures_util::Stream;
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
) -> Result<Box<dyn querymt::chat::ChatResponse>, anyhow::Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Box<dyn querymt::chat::ChatResponse>, LLMError>>,
{
    let max_retries = config.execution_policy.rate_limit.max_retries;
    let mut attempt = 0;
    Span::current().record("rate_limited", false);

    loop {
        attempt += 1;

        if cancel_token.is_cancelled() {
            return Err(anyhow::anyhow!("Cancelled"));
        }

        match call_fn().await {
            Ok(response) => {
                Span::current().record("attempt", attempt);
                return Ok(response);
            }
            Err(e) => {
                if let Some((message, retry_after)) = extract_rate_limit_info(&e) {
                    Span::current().record("rate_limited", true);
                    if attempt >= max_retries {
                        return Err(anyhow::anyhow!(
                            "Rate limit exceeded after {} attempts: {}",
                            max_retries,
                            message
                        ));
                    }

                    let wait_secs = calculate_rate_limit_wait(config, retry_after, attempt);
                    let started_at = time::OffsetDateTime::now_utc().unix_timestamp();

                    info!(
                        "Session {} rate limited, attempt {}/{}, waiting {}s",
                        session_id, attempt, max_retries, wait_secs
                    );

                    config.emit_event(
                        session_id,
                        AgentEventKind::RateLimited {
                            message: message.clone(),
                            wait_secs,
                            started_at,
                            attempt,
                            max_attempts: max_retries,
                        },
                    );

                    let cancelled = wait_with_cancellation(wait_secs, cancel_token).await;
                    if cancelled {
                        debug!(
                            "Rate limit wait cancelled for session {} during attempt {}",
                            session_id, attempt
                        );
                        return Err(anyhow::anyhow!("Cancelled during rate limit wait"));
                    }

                    info!(
                        "Session {} resuming after rate limit wait, attempt {}",
                        session_id,
                        attempt + 1
                    );

                    config.emit_event(
                        session_id,
                        AgentEventKind::RateLimitResume {
                            attempt: attempt + 1,
                        },
                    );

                    continue;
                } else {
                    Span::current().record("rate_limited", false);
                    // Convert non-rate-limit errors to anyhow::Error
                    return Err(anyhow::Error::from(e));
                }
            }
        }
    }
}

/// Calculate how long to wait before retrying after a rate limit.
fn calculate_rate_limit_wait(
    config: &AgentConfig,
    retry_after: Option<u64>,
    attempt: usize,
) -> u64 {
    match retry_after {
        Some(secs) => secs,
        None => {
            let base = config.execution_policy.rate_limit.default_wait_secs as f64;
            let multiplier = config.execution_policy.rate_limit.backoff_multiplier;
            (base * multiplier.powi((attempt - 1) as i32)) as u64
        }
    }
}

/// Wait for a duration with support for cancellation.
///
/// Returns `true` if cancelled, `false` if the wait completed normally.
#[instrument(name = "agent.llm.rate_limit_wait", skip(cancel_token), fields(wait_secs = wait_secs))]
async fn wait_with_cancellation(wait_secs: u64, cancel_token: &CancellationToken) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_secs(wait_secs)) => {
            false
        }
        _ = cancel_token.cancelled() => {
            true
        }
    }
}

/// Extract rate limit information from an error.
///
/// Returns `Some((message, retry_after_seconds))` if the error is a rate limit error.
fn extract_rate_limit_info(error: &LLMError) -> Option<(String, Option<u64>)> {
    match error {
        LLMError::RateLimited {
            message,
            retry_after_secs,
        } => Some((message.clone(), *retry_after_secs)),
        _ => None,
    }
}

/// Create a streaming connection with retry logic for rate limits and transient errors.
///
/// Unlike `call_llm_with_retry` which retries the entire request including consuming the
/// response, this only retries stream *creation*. Once the stream yields its first chunk
/// we are committed — tokens may already have been sent to the UI and we cannot roll back.
pub(super) async fn create_stream_with_retry<F, Fut>(
    config: &AgentConfig,
    session_id: &str,
    cancel_token: &CancellationToken,
    create_stream: F,
) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, LLMError>> + Send>>, anyhow::Error>
where
    F: Fn() -> Fut,
    Fut: Future<
        Output = Result<
            Pin<Box<dyn Stream<Item = Result<StreamChunk, LLMError>> + Send>>,
            LLMError,
        >,
    >,
{
    let max_retries = config.execution_policy.rate_limit.max_retries;
    let mut attempt = 0;

    loop {
        attempt += 1;

        if cancel_token.is_cancelled() {
            return Err(anyhow::anyhow!("Cancelled"));
        }

        match create_stream().await {
            Ok(stream) => return Ok(stream),

            Err(e) => {
                if let Some((message, retry_after)) = extract_rate_limit_info(&e) {
                    if attempt >= max_retries {
                        return Err(anyhow::anyhow!(
                            "Rate limit exceeded after {} attempts: {}",
                            max_retries,
                            message
                        ));
                    }

                    let wait_secs = calculate_rate_limit_wait(config, retry_after, attempt);
                    let started_at = time::OffsetDateTime::now_utc().unix_timestamp();

                    info!(
                        "Session {} rate limited (streaming), attempt {}/{}, waiting {}s",
                        session_id, attempt, max_retries, wait_secs
                    );

                    config.emit_event(
                        session_id,
                        AgentEventKind::RateLimited {
                            message: message.clone(),
                            wait_secs,
                            started_at,
                            attempt,
                            max_attempts: max_retries,
                        },
                    );

                    let cancelled = wait_with_cancellation(wait_secs, cancel_token).await;
                    if cancelled {
                        return Err(anyhow::anyhow!("Cancelled during rate limit wait"));
                    }

                    config.emit_event(
                        session_id,
                        AgentEventKind::RateLimitResume {
                            attempt: attempt + 1,
                        },
                    );

                    continue;
                } else if is_transient_error(&e) && attempt < max_retries {
                    // Transient errors get exponential backoff
                    let delay_secs = config.execution_policy.rate_limit.default_wait_secs
                        * 2u64.saturating_pow(attempt as u32 - 1);
                    debug!(
                        "Session {} transient stream error on attempt {}, retrying in {}s: {}",
                        session_id, attempt, delay_secs, e
                    );
                    let cancelled = wait_with_cancellation(delay_secs, cancel_token).await;
                    if cancelled {
                        return Err(anyhow::anyhow!("Cancelled during retry wait"));
                    }
                    continue;
                } else {
                    return Err(anyhow::Error::from(e));
                }
            }
        }
    }
}

/// Returns true for errors that are worth retrying (connection-level failures).
fn is_transient_error(e: &LLMError) -> bool {
    matches!(e, LLMError::HttpError(_))
}

// ══════════════════════════════════════════════════════════════════════════
//  Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_config::AgentConfig;
    use crate::agent::core::{
        AgentMode, DelegationContextConfig, DelegationContextTiming, SnapshotPolicy, ToolConfig,
        ToolPolicy,
    };
    use crate::config::RuntimeExecutionPolicy;
    use crate::delegation::DefaultAgentRegistry;
    use crate::event_bus::EventBus;
    use crate::index::{WorkspaceIndexManagerActor, WorkspaceIndexManagerConfig};
    use crate::session::store::SessionStore;
    use crate::test_utils::{
        MockLlmProvider, MockSessionStore, SharedLlmProvider, TestProviderFactory, mock_llm_config,
        mock_plugin_registry, mock_session,
    };
    use crate::tools::ToolRegistry;
    use querymt::LLMParams;
    use querymt::chat::ChatResponse;
    use querymt::error::LLMError;
    use std::collections::{HashMap, HashSet};
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
        let factory = Arc::new(TestProviderFactory { provider: shared });
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

        let store: Arc<dyn SessionStore> = Arc::new(store);
        let provider_ctx = Arc::new(crate::session::provider::SessionProvider::new(
            Arc::new(plugin_registry),
            store.clone(),
            LLMParams::new().provider("mock").model("mock-model"),
        ));

        let mut policy = RuntimeExecutionPolicy::default();
        policy.rate_limit.max_retries = 3;
        policy.rate_limit.default_wait_secs = 1;
        policy.rate_limit.backoff_multiplier = 2.0;

        let config = Arc::new(AgentConfig {
            provider: provider_ctx,
            event_bus: Arc::new(EventBus::new()),
            agent_registry: Arc::new(DefaultAgentRegistry::new()),
            workspace_manager_actor: WorkspaceIndexManagerActor::new(
                WorkspaceIndexManagerConfig::default(),
            ),
            default_mode: Arc::new(std::sync::Mutex::new(AgentMode::Build)),
            tool_config: ToolConfig {
                policy: ToolPolicy::ProviderOnly,
                ..ToolConfig::default()
            },
            tool_registry: ToolRegistry::new(),
            middleware_drivers: Vec::new(),
            auth_methods: Vec::new(),
            max_steps: None,
            snapshot_policy: SnapshotPolicy::None,
            assume_mutating: true,
            mutating_tools: HashSet::new(),
            max_prompt_bytes: None,
            execution_timeout_secs: 300,
            delegation_wait_policy: crate::config::DelegationWaitPolicy::default(),
            delegation_wait_timeout_secs: 120,
            delegation_cancel_grace_secs: 5,
            execution_policy: policy,
            compaction: crate::session::compaction::SessionCompaction::new(),
            snapshot_backend: None,
            snapshot_gc_config: crate::snapshot::GcConfig::default(),
            delegation_context_config: DelegationContextConfig {
                timing: DelegationContextTiming::FirstTurnOnly,
                auto_inject: true,
            },
            pending_elicitations: Arc::new(Mutex::new(HashMap::new())),
            mcp_servers: Vec::new(),
        });

        (config, temp_dir)
    }

    // ── extract_rate_limit_info tests ────────────────────────────────────────

    #[test]
    fn test_extract_rate_limit_info_rate_limited_with_retry_after() {
        let err = LLMError::RateLimited {
            message: "Too many requests".to_string(),
            retry_after_secs: Some(30),
        };
        let info = extract_rate_limit_info(&err);
        assert!(info.is_some());
        let (msg, retry_after) = info.unwrap();
        assert_eq!(msg, "Too many requests");
        assert_eq!(retry_after, Some(30));
    }

    #[test]
    fn test_extract_rate_limit_info_rate_limited_no_retry_after() {
        let err = LLMError::RateLimited {
            message: "Quota exceeded".to_string(),
            retry_after_secs: None,
        };
        let info = extract_rate_limit_info(&err);
        assert!(info.is_some());
        let (msg, retry_after) = info.unwrap();
        assert_eq!(msg, "Quota exceeded");
        assert!(retry_after.is_none());
    }

    #[test]
    fn test_extract_rate_limit_info_non_rate_limit_returns_none() {
        let err = LLMError::GenericError("something broke".to_string());
        assert!(extract_rate_limit_info(&err).is_none());

        let err = LLMError::HttpError("connection refused".to_string());
        assert!(extract_rate_limit_info(&err).is_none());
    }

    // ── calculate_rate_limit_wait tests ─────────────────────────────────────

    #[tokio::test]
    async fn test_calculate_rate_limit_wait_uses_retry_after() {
        let (config, _temp) = make_config().await;
        let wait = calculate_rate_limit_wait(&config, Some(60), 1);
        assert_eq!(wait, 60, "should use retry_after_secs directly");
    }

    #[tokio::test]
    async fn test_calculate_rate_limit_wait_exponential_backoff_no_retry_after() {
        let (config, _temp) = make_config().await;
        // attempt=1: base * 2^0 = 1 * 1.0 = 1s
        let w1 = calculate_rate_limit_wait(&config, None, 1);
        // attempt=2: base * 2^1 = 1 * 2.0 = 2s
        let w2 = calculate_rate_limit_wait(&config, None, 2);
        // attempt=3: base * 2^2 = 1 * 4.0 = 4s
        let w3 = calculate_rate_limit_wait(&config, None, 3);
        assert!(w2 >= w1, "wait should grow with attempt");
        assert!(w3 >= w2, "wait should grow with attempt");
    }

    // ── is_transient_error tests ─────────────────────────────────────────────

    #[test]
    fn test_is_transient_error_http_error() {
        let err = LLMError::HttpError("connection reset".to_string());
        assert!(is_transient_error(&err));
    }

    #[test]
    fn test_is_transient_error_non_transient() {
        assert!(!is_transient_error(&LLMError::GenericError(
            "oops".to_string()
        )));
        assert!(!is_transient_error(&LLMError::RateLimited {
            message: "rate".to_string(),
            retry_after_secs: None,
        }));
    }

    // ── wait_with_cancellation tests ─────────────────────────────────────────

    #[tokio::test]
    async fn test_wait_with_cancellation_completes_normally() {
        let token = CancellationToken::new();
        // Very short wait (0s) should complete normally
        let cancelled = wait_with_cancellation(0, &token).await;
        assert!(
            !cancelled,
            "wait should complete normally, not be cancelled"
        );
    }

    #[tokio::test]
    async fn test_wait_with_cancellation_cancelled_early() {
        let token = CancellationToken::new();
        token.cancel(); // cancel before waiting
        let cancelled = wait_with_cancellation(60, &token).await;
        assert!(cancelled, "should return true when already cancelled");
    }

    #[tokio::test]
    async fn test_wait_with_cancellation_cancel_during_wait() {
        let token = CancellationToken::new();
        let token_clone = token.clone();
        // Cancel after 10ms while waiting for 60s
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            token_clone.cancel();
        });
        let cancelled = wait_with_cancellation(60, &token).await;
        assert!(cancelled, "should be cancelled by background task");
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

        let result = call_llm_with_retry(&config, "test-session", &token, || async {
            Err::<Box<dyn ChatResponse>, _>(LLMError::GenericError("fatal error".to_string()))
        })
        .await;

        // Non-rate-limit errors should fail immediately without retrying
        assert!(result.is_err());
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

        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Cancelled"),
            "expected Cancelled, got: {}",
            msg
        );
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

        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Rate limit exceeded"),
            "unexpected error: {}",
            msg
        );
        // Should have tried max_retries times
        assert_eq!(call_count.load(Ordering::SeqCst), 3);
    }
}
