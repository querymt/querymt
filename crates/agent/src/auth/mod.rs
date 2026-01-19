//! OAuth token management for the agent crate
//!
//! This module provides functionality to read and refresh OAuth tokens from the system keyring.
//! It shares the same keyring storage as the CLI (`qmt auth login`) for seamless integration.
//!
//! # Features
//!
//! - Read OAuth tokens from system keyring
//! - Automatic token refresh when expired
//! - Compatible with CLI's token storage
//!
//! # Usage
//!
//! Enable the `oauth` feature in your `Cargo.toml`:
//!
//! ```toml
//! [dependencies]
//! qmt-agent = { version = "0.1", features = ["oauth"] }
//! ```
//!
//! The OAuth tokens are automatically used when building providers:
//!
//! ```rust,no_run
//! # use querymt_agent::prelude::*;
//! # async fn example() -> anyhow::Result<()> {
//! // Tokens from `qmt auth login anthropic` are automatically used
//! let agent = Agent::single()
//!     .provider("anthropic", "claude-sonnet-4-20250514")
//!     .build()
//!     .await?;
//! # Ok(())
//! # }
//! ```

pub mod provider;
pub mod store;

pub use provider::{TokenRefresher, get_token_refresher};
pub use store::SecretStore;

use anyhow::{Result, anyhow};

/// Get a valid OAuth token for a provider, refreshing if necessary
///
/// This function attempts to retrieve a valid OAuth token from the system keyring.
/// If the token is expired, it automatically refreshes it using the refresh token.
///
/// # Arguments
///
/// * `provider` - The provider name (e.g., "anthropic", "openai")
///
/// # Returns
///
/// * `Result<String>` - The access token or an error
///
/// # Errors
///
/// Returns an error if:
/// - Keyring access fails
/// - No tokens found for the provider
/// - Token refresh fails
///
/// # Example
///
/// ```rust,no_run
/// # use querymt_agent::auth::get_or_refresh_token;
/// # async fn example() -> anyhow::Result<()> {
/// let token = get_or_refresh_token("anthropic").await?;
/// println!("Got access token: {}", token);
/// # Ok(())
/// # }
/// ```
pub async fn get_or_refresh_token(provider: &str) -> Result<String> {
    log::debug!("Checking OAuth tokens for provider: {}", provider);

    let store = SecretStore::new().map_err(|e| anyhow!("Keyring access failed: {}", e))?;

    // Try to get valid (non-expired) token
    if let Some(token) = store.get_valid_access_token(provider) {
        log::debug!("Found valid OAuth token for: {}", provider);
        return Ok(token);
    }

    // Token is expired or missing - try to refresh
    if let Some(tokens) = store.get_oauth_tokens(provider) {
        log::debug!("OAuth token expired for {}, attempting refresh", provider);

        let refresher = get_token_refresher(provider)?;
        let new_tokens = refresher
            .refresh_token(&tokens.refresh_token)
            .await
            .map_err(|e| {
                log::debug!("OAuth refresh failed for {}: {}", provider, e);
                anyhow!("Token refresh failed: {}", e)
            })?;

        log::info!("Refreshed OAuth token for provider: {}", provider);

        // Store the refreshed tokens
        let mut store = SecretStore::new()?;
        store.set_oauth_tokens(provider, &new_tokens)?;

        Ok(new_tokens.access_token)
    } else {
        log::debug!("No OAuth tokens in keyring for: {}", provider);
        Err(anyhow!("No OAuth tokens found for {}", provider))
    }
}
