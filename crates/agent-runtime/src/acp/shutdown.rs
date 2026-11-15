//! Graceful shutdown signal handling for ACP servers
//!
//! This module provides utilities for handling SIGTERM and SIGINT (Ctrl+C)
//! signals to allow graceful shutdown of ACP servers.

use tokio::signal;

/// Wait for a shutdown signal (SIGTERM or SIGINT/Ctrl+C).
///
/// This function will complete when either:
/// - SIGINT (Ctrl+C) is received
/// - SIGTERM is received (Unix only)
///
/// # Example
///
/// ```no_run
/// use querymt_agent::acp::shutdown;
///
/// # async fn example() {
/// tokio::select! {
///     result = run_server() => {
///         println!("Server completed: {:?}", result);
///     }
///     _ = shutdown::signal() => {
///         println!("Shutdown signal received");
///     }
/// }
/// # }
/// # async fn run_server() -> Result<(), Box<dyn std::error::Error>> { Ok(()) }
/// ```
pub async fn signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            log::info!("Received SIGINT (Ctrl+C), initiating graceful shutdown...");
        },
        _ = terminate => {
            log::info!("Received SIGTERM, initiating graceful shutdown...");
        },
    }
}

/// Create a shutdown signal future that can be used with `axum::serve`
///
/// # Example
///
/// ```no_run
/// use querymt_agent::acp::shutdown;
/// use axum::Router;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let app = Router::new();
/// let listener = tokio::net::TcpListener::bind("127.0.0.1:3030").await?;
///
/// axum::serve(listener, app)
///     .with_graceful_shutdown(shutdown::signal())
///     .await?;
/// # Ok(())
/// # }
/// ```
pub fn graceful_shutdown() -> impl std::future::Future<Output = ()> {
    signal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_signal_does_not_complete_immediately() {
        // The signal future should not complete without a signal
        let result = tokio::time::timeout(Duration::from_millis(100), signal()).await;

        assert!(
            result.is_err(),
            "Signal future should timeout without signal"
        );
    }
}
