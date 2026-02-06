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

// Re-export from querymt-utils
pub use querymt_utils::oauth::{
    OAuthFlowData, OAuthMode, OAuthProvider, OAuthUI, TokenSet, authenticate,
    extract_code_from_query, get_oauth_provider, get_or_refresh_token, get_valid_token,
    openai_callback_server, refresh_tokens, show_auth_status,
};
pub use querymt_utils::secret_store::SecretStore;
