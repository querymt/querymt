//! LLM retry logic with rate limit handling
//!
//! This module handles retrying LLM calls with exponential backoff when rate limits are hit.

use crate::agent::agent_config::AgentConfig;
use crate::events::AgentEventKind;
use log::{debug, info};
use querymt::error::LLMError;
use tokio::sync::watch;

/// Call an LLM with automatic retry on rate limit errors.
///
/// This function wraps any LLM call and automatically retries with exponential backoff
/// when rate limit errors are detected. It respects cancellation via the `cancel_rx` channel.
pub(super) async fn call_llm_with_retry<F, Fut>(
    config: &AgentConfig,
    session_id: &str,
    cancel_rx: &watch::Receiver<bool>,
    mut call_fn: F,
) -> Result<Box<dyn querymt::chat::ChatResponse>, anyhow::Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Box<dyn querymt::chat::ChatResponse>, LLMError>>,
{
    let max_retries = config.rate_limit_config.max_retries;
    let mut attempt = 0;

    loop {
        attempt += 1;

        if *cancel_rx.borrow() {
            return Err(anyhow::anyhow!("Cancelled"));
        }

        match call_fn().await {
            Ok(response) => return Ok(response),
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

                    let cancelled = wait_with_cancellation(wait_secs, cancel_rx).await;
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
            let base = config.rate_limit_config.default_wait_secs as f64;
            let multiplier = config.rate_limit_config.backoff_multiplier;
            (base * multiplier.powi((attempt - 1) as i32)) as u64
        }
    }
}

/// Wait for a duration with support for cancellation.
///
/// Returns `true` if cancelled, `false` if the wait completed normally.
async fn wait_with_cancellation(wait_secs: u64, cancel_rx: &watch::Receiver<bool>) -> bool {
    let mut cancel_rx = cancel_rx.clone();

    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_secs(wait_secs)) => {
            false
        }
        _ = cancel_rx.changed() => {
            *cancel_rx.borrow()
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
