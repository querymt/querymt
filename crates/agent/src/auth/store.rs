//! Secret storage for OAuth tokens using system keyring
//!
//! This module provides a subset of the CLI's SecretStore functionality,
//! focused on reading and updating OAuth tokens. It shares the same keyring
//! service name ("querymt-cli") for compatibility.

use anthropic_auth::TokenSet;
use keyring::Entry;
use std::io;

/// Service name for keyring entries (shared with CLI)
const SERVICE_NAME: &str = "querymt-cli";

/// A secure storage for OAuth tokens using system keyring
#[derive(Debug)]
pub struct SecretStore {
    // Stateless - all operations go directly to keyring
}

impl SecretStore {
    /// Creates a new SecretStore instance
    pub fn new() -> io::Result<Self> {
        Ok(SecretStore {})
    }

    /// Retrieves a secret value for the given key
    ///
    /// # Arguments
    ///
    /// * `key` - The key to look up
    ///
    /// # Returns
    ///
    /// * `Option<String>` - The secret value if found, or None
    pub fn get(&self, key: &str) -> Option<String> {
        let entry = Entry::new(SERVICE_NAME, key).ok()?;
        entry.get_password().ok()
    }

    /// Sets a secret value for the given key
    ///
    /// # Arguments
    ///
    /// * `key` - The key to store the secret under
    /// * `value` - The secret value to store
    ///
    /// # Returns
    ///
    /// * `io::Result<()>` - Success or an IO error
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) -> io::Result<()> {
        let key = key.into();
        let value = value.into();

        let entry = Entry::new(SERVICE_NAME, &key).map_err(|e| io::Error::other(e.to_string()))?;

        entry
            .set_password(&value)
            .map_err(|e| io::Error::other(e.to_string()))
    }

    /// Sets OAuth tokens for a provider
    ///
    /// # Arguments
    ///
    /// * `provider` - The provider name (e.g., "anthropic", "openai")
    /// * `tokens` - The OAuth token set
    ///
    /// # Returns
    ///
    /// * `io::Result<()>` - Success or an IO error
    pub fn set_oauth_tokens(&mut self, provider: &str, tokens: &TokenSet) -> io::Result<()> {
        let tokens_json = serde_json::to_string(tokens)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        self.set(format!("oauth_{}", provider), tokens_json)
    }

    /// Retrieves OAuth tokens for a provider
    ///
    /// # Arguments
    ///
    /// * `provider` - The provider name (e.g., "anthropic", "openai")
    ///
    /// # Returns
    ///
    /// * `Option<TokenSet>` - The OAuth token set if found, or None
    pub fn get_oauth_tokens(&self, provider: &str) -> Option<TokenSet> {
        let tokens_json = self.get(&format!("oauth_{}", provider))?;
        serde_json::from_str(&tokens_json).ok()
    }

    /// Gets the access token for a provider, returning None if expired
    ///
    /// # Arguments
    ///
    /// * `provider` - The provider name
    ///
    /// # Returns
    ///
    /// * `Option<String>` - The access token if valid, or None if expired/missing
    pub fn get_valid_access_token(&self, provider: &str) -> Option<String> {
        let tokens = self.get_oauth_tokens(provider)?;
        if tokens.is_expired() {
            None
        } else {
            Some(tokens.access_token)
        }
    }

    /// Checks if OAuth tokens are expired
    ///
    /// # Arguments
    ///
    /// * `provider` - The provider name
    ///
    /// # Returns
    ///
    /// * `bool` - True if tokens are expired or not found, false otherwise
    pub fn are_tokens_expired(&self, provider: &str) -> bool {
        if let Some(tokens) = self.get_oauth_tokens(provider) {
            tokens.is_expired()
        } else {
            true
        }
    }
}

impl Default for SecretStore {
    fn default() -> Self {
        Self::new().expect("Failed to create SecretStore")
    }
}
