//! Credential resolution for LLM providers.
//!
//! This module provides the [`ApiKeyResolver`] trait for dynamic credential
//! resolution. It bridges the async/sync boundary that exists between the
//! adapter layer (async) and HTTP provider request builders (sync):
//!
//! 1. The adapter calls [`ApiKeyResolver::resolve()`] from async context
//!    before each request to ensure the credential is fresh.
//! 2. The provider calls [`ApiKeyResolver::current()`] from sync context
//!    (inside `chat_request()`, `embed_request()`, etc.) to read the
//!    most recently resolved value.
//!
//! # Implementations
//!
//! - [`StaticKeyResolver`]: Returns a fixed credential. Used for environment
//!   variable API keys that don't expire.
//!
//! For OAuth-based resolvers that refresh tokens, see the `oauth` feature
//! in the agent crate.

use crate::error::LLMError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Resolves API credentials at request time, supporting refresh/rotation.
///
/// This trait uses a two-phase design to bridge the async/sync boundary:
///
/// - [`resolve()`](ApiKeyResolver::resolve): Async — ensures the credential
///   is fresh. May perform I/O (keyring access, HTTP token refresh). Called
///   by the adapter layer before each outbound request.
///
/// - [`current()`](ApiKeyResolver::current): Sync — returns the most recently
///   resolved credential. Called by provider code inside sync request builders
///   like `chat_request()`.
///
/// # Contract
///
/// Callers **must** call `resolve()` before relying on `current()`. Calling
/// `current()` without a prior `resolve()` returns whatever value was set at
/// construction time (which may be stale or empty for OAuth resolvers).
pub trait ApiKeyResolver: Send + Sync + std::fmt::Debug {
    /// Ensure the credential is fresh.
    ///
    /// For static keys this is a no-op. For OAuth tokens this may refresh
    /// an expired token from the system keyring or an authorization server.
    ///
    /// Called from async context before each outbound request.
    fn resolve(&self) -> Pin<Box<dyn Future<Output = Result<(), LLMError>> + Send + '_>>;

    /// Return the most recently resolved credential.
    ///
    /// This is synchronous and cheap. Implementations should use interior
    /// mutability (e.g., `RwLock`) to make the value set by `resolve()`
    /// available here.
    fn current(&self) -> String;
}

/// A resolver that always returns the same fixed credential.
///
/// Used for API keys sourced from environment variables or static configuration
/// that don't expire or need refresh.
#[derive(Clone)]
pub struct StaticKeyResolver(String);

impl StaticKeyResolver {
    /// Create a new resolver with a fixed credential value.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }
}

impl std::fmt::Debug for StaticKeyResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't leak the actual key in debug output
        f.debug_struct("StaticKeyResolver")
            .field("key", &"<redacted>")
            .finish()
    }
}

impl ApiKeyResolver for StaticKeyResolver {
    fn resolve(&self) -> Pin<Box<dyn Future<Output = Result<(), LLMError>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn current(&self) -> String {
        self.0.clone()
    }
}

/// Convenience function to create a resolver from a static key.
pub fn static_key(key: impl Into<String>) -> Arc<dyn ApiKeyResolver> {
    Arc::new(StaticKeyResolver::new(key))
}
