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
//! querymt-agent = { version = "0.1", features = ["oauth"] }
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

pub mod service;

// Re-export AuthMethod from session::provider (the canonical definition)
// so that UI and handler code can access it via `crate::auth::AuthMethod`.
pub use crate::session::provider::AuthMethod;

// Re-export SecretStore unconditionally (used for plain API-key storage too).
pub use querymt_utils::secret_store::SecretStore;

// OAuth-specific re-exports — only available with the `oauth` feature.
#[cfg(feature = "oauth")]
pub use querymt_utils::OAuthFlowKind;
#[cfg(feature = "oauth")]
pub use querymt_utils::oauth::{
    OAuthFlowData, OAuthMode, OAuthProvider, OAuthUI, TokenSet, anthropic_callback_server,
    authenticate, extract_code_from_query, get_oauth_provider, get_or_refresh_token,
    get_valid_token, openai_callback_server, refresh_tokens, show_auth_status,
};

#[cfg(feature = "oauth")]
use querymt::auth::ApiKeyResolver;
#[cfg(feature = "oauth")]
use querymt::error::LLMError;
#[cfg(feature = "oauth")]
use std::future::Future;
#[cfg(feature = "oauth")]
use std::pin::Pin;

/// Resolves API credentials by refreshing OAuth tokens from the system keyring.
///
/// On each [`resolve()`](ApiKeyResolver::resolve) call, this resolver invokes
/// [`get_or_refresh_token`] which:
/// - Returns the cached token if it's still valid
/// - Refreshes an expired token using the stored refresh token
/// - Fails if no OAuth session exists for the provider
///
/// The resolved token is stored internally and returned by
/// [`current()`](ApiKeyResolver::current) for synchronous access in
/// provider request builders.
///
/// # Example
///
/// ```rust,no_run
/// use querymt_agent::auth::OAuthKeyResolver;
///
/// let resolver = OAuthKeyResolver::new("anthropic", "sk-ant-oat01-initial-token");
/// // resolver.resolve() will refresh the token when called
/// // resolver.current() returns the most recently resolved token
/// ```
#[cfg(feature = "oauth")]
pub struct OAuthKeyResolver {
    provider_name: String,
    cached_key: std::sync::RwLock<String>,
}

#[cfg(feature = "oauth")]
impl OAuthKeyResolver {
    /// Create a new OAuth resolver for the given provider.
    ///
    /// `initial_key` is the token obtained at provider construction time.
    /// It will be returned by `current()` until the first `resolve()` call
    /// updates it.
    pub fn new(provider_name: impl Into<String>, initial_key: impl Into<String>) -> Self {
        Self {
            provider_name: provider_name.into(),
            cached_key: std::sync::RwLock::new(initial_key.into()),
        }
    }
}

#[cfg(feature = "oauth")]
impl std::fmt::Debug for OAuthKeyResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthKeyResolver")
            .field("provider_name", &self.provider_name)
            .field("cached_key", &"<redacted>")
            .finish()
    }
}

#[cfg(feature = "oauth")]
impl ApiKeyResolver for OAuthKeyResolver {
    fn resolve(&self) -> Pin<Box<dyn Future<Output = Result<(), LLMError>> + Send + '_>> {
        Box::pin(async {
            let token = get_or_refresh_token(&self.provider_name)
                .await
                .map_err(|e| LLMError::AuthError(format!("OAuth refresh failed: {}", e)))?;
            *self.cached_key.write().unwrap() = token;
            Ok(())
        })
    }

    fn current(&self) -> String {
        self.cached_key.read().unwrap().clone()
    }
}
