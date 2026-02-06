//! Secret storage for API keys and OAuth tokens using system keyring
//!
//! This module provides secure storage for sensitive information using the system's native
//! keyring service (Keychain on macOS, Credential Manager on Windows, Secret Service on Linux).
//!
//! # Examples
//!
//! ```rust,no_run
//! use querymt_utils::secret_store::SecretStore;
//!
//! # fn main() -> std::io::Result<()> {
//! let mut store = SecretStore::new()?;
//!
//! // Store an API key
//! store.set("api_key", "sk-...")?;
//!
//! // Retrieve it
//! if let Some(key) = store.get("api_key") {
//!     println!("Found API key");
//! }
//!
//! // Delete it
//! store.delete("api_key")?;
//! # Ok(())
//! # }
//! ```

use anthropic_auth::TokenSet;
use keyring::Entry;
use std::io;
use std::time::SystemTime;

/// Key used to store the default provider in the secret store
const DEFAULT_PROVIDER_KEY: &str = "default";

/// Service name for keyring entries (shared across CLI and agent)
const SERVICE_NAME: &str = "querymt-cli";

/// A secure storage for API keys and other sensitive information using system keyring
#[derive(Debug)]
pub struct SecretStore {
    // Stateless - all operations go directly to keyring
}

impl SecretStore {
    /// Creates a new SecretStore instance
    ///
    /// # Returns
    ///
    /// * `io::Result<Self>` - A new SecretStore instance or an IO error
    pub fn new() -> io::Result<Self> {
        Ok(SecretStore {})
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

    /// Deletes a secret with the given key
    ///
    /// # Arguments
    ///
    /// * `key` - The key to delete
    ///
    /// # Returns
    ///
    /// * `io::Result<()>` - Success or an IO error
    pub fn delete(&mut self, key: &str) -> io::Result<()> {
        let entry = Entry::new(SERVICE_NAME, key).map_err(|e| io::Error::other(e.to_string()))?;

        entry
            .delete_credential()
            .map_err(|e| io::Error::other(e.to_string()))
    }

    /// Sets the default provider for LLM interactions
    ///
    /// # Arguments
    ///
    /// * `provider` - The provider string in format "provider:model"
    ///
    /// # Returns
    ///
    /// * `io::Result<()>` - Success or an IO error
    pub fn set_default_provider(&mut self, provider: &str) -> io::Result<()> {
        self.set(DEFAULT_PROVIDER_KEY, provider)
    }

    /// Retrieves the default provider for LLM interactions
    ///
    /// # Returns
    ///
    /// * `Option<String>` - The default provider if set, or None
    pub fn get_default_provider(&self) -> Option<String> {
        self.get(DEFAULT_PROVIDER_KEY)
    }

    /// Deletes the default provider setting
    ///
    /// # Returns
    ///
    /// * `io::Result<()>` - Success or an IO error
    pub fn delete_default_provider(&mut self) -> io::Result<()> {
        self.delete(DEFAULT_PROVIDER_KEY)
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

    /// Deletes OAuth tokens for a provider
    ///
    /// # Arguments
    ///
    /// * `provider` - The provider name
    ///
    /// # Returns
    ///
    /// * `io::Result<()>` - Success or an IO error
    pub fn delete_oauth_tokens(&mut self, provider: &str) -> io::Result<()> {
        self.delete(&format!("oauth_{}", provider))
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

    /// Gets the refresh token for a provider
    ///
    /// # Arguments
    ///
    /// * `provider` - The provider name
    ///
    /// # Returns
    ///
    /// * `Option<String>` - The refresh token if found, or None
    pub fn get_refresh_token(&self, provider: &str) -> Option<String> {
        let tokens = self.get_oauth_tokens(provider)?;
        Some(tokens.refresh_token.clone())
    }
}

impl Default for SecretStore {
    fn default() -> Self {
        Self::new().expect("Failed to create SecretStore")
    }
}

/// Helper to format timestamp for display
///
/// # Arguments
///
/// * `timestamp` - Unix timestamp in seconds
///
/// # Returns
///
/// * `String` - A human-readable string like "in 2h 30m" or "expired"
pub fn format_timestamp(timestamp: u64) -> String {
    let duration = std::time::Duration::from_secs(timestamp);
    let datetime = SystemTime::UNIX_EPOCH + duration;

    match datetime.duration_since(SystemTime::now()) {
        Ok(remaining) => {
            let hours = remaining.as_secs() / 3600;
            let minutes = (remaining.as_secs() % 3600) / 60;
            format!("in {}h {}m", hours, minutes)
        }
        Err(_) => "expired".to_string(),
    }
}
