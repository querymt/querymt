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
