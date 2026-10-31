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

#[cfg(feature = "oauth")]
use anthropic_auth::TokenSet;
use keyring_core::Entry;
use std::io;
use std::sync::OnceLock;
use std::time::SystemTime;

/// Ensures the default keyring store is initialized exactly once.
///
/// On first call this sets the platform-native credential store as the default
/// used by `keyring_core::Entry`. Subsequent calls are no-ops.
static STORE_INIT: OnceLock<()> = OnceLock::new();

fn ensure_store_initialized() {
    STORE_INIT.get_or_init(|| {
        init_native_store();
    });
}

/// Platform-aware store initialization.
///
/// Creates the appropriate native credential store and sets it as the
/// default for `keyring_core::Entry::new()`.
///
/// # Panics
///
/// Panics if the platform store cannot be created (e.g. keychain access failure).
fn init_native_store() {
    #[cfg(target_os = "macos")]
    {
        let store = apple_native_keyring_store::keychain::Store::new()
            .expect("Failed to create macOS Keychain store");
        keyring_core::set_default_store(store);
    }

    #[cfg(target_os = "windows")]
    {
        let store = windows_native_keyring_store::Store::new()
            .expect("Failed to create Windows Credential store");
        keyring_core::set_default_store(store);
    }

    // On Linux: use DBus Secret Service when the feature is enabled,
    // otherwise fall back to kernel keyutils.
    #[cfg(all(target_os = "linux", feature = "dbus-secret-service"))]
    {
        let store = dbus_secret_service_keyring_store::Store::new()
            .expect("Failed to create DBus Secret Service store");
        keyring_core::set_default_store(store);
    }

    #[cfg(all(target_os = "linux", not(feature = "dbus-secret-service")))]
    {
        let store = linux_keyutils_keyring_store::Store::new()
            .expect("Failed to create Linux keyutils store");
        keyring_core::set_default_store(store);
    }
}

/// Install the Android host-backed keyring store before any secret operations.
#[cfg(target_os = "android")]
pub fn install_android_default_store(store: std::sync::Arc<keyring_core::CredentialStore>) {
    keyring_core::set_default_store(store);
}

/// Convert a keyring error into an `io::Error`, preserving the distinction
/// between "the OS refused access" (`PermissionDenied`) and other failures.
fn map_keyring_error(error: keyring_core::Error) -> io::Error {
    match error {
        keyring_core::Error::NoStorageAccess(_) => {
            io::Error::new(io::ErrorKind::PermissionDenied, error.to_string())
        }
        other => io::Error::other(other.to_string()),
    }
}

/// Key used to store the default provider in the secret store
const DEFAULT_PROVIDER_KEY: &str = "default";

/// Service name for keyring entries (shared across CLI and agent)
const SERVICE_NAME: &str = "querymt";

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
        ensure_store_initialized();
        #[cfg(target_os = "android")]
        if keyring_core::get_default_store().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Android credential store is not configured",
            ));
        }
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

        let entry = Entry::new(SERVICE_NAME, &key).map_err(map_keyring_error)?;

        entry.set_password(&value).map_err(map_keyring_error)
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
    ///
    /// Backend failures (for example a denied keychain prompt) are logged at
    /// `warn` level and reported as `None`. Use [`SecretStore::try_get`] when
    /// the caller needs to distinguish "missing" from "unreadable".
    pub fn get(&self, key: &str) -> Option<String> {
        match self.try_get(key) {
            Ok(value) => value,
            Err(error) => {
                log::warn!("Failed to read secret '{key}' from the system keyring: {error}");
                None
            }
        }
    }

    /// Retrieves a secret while distinguishing a missing key from backend failure.
    pub fn try_get(&self, key: &str) -> io::Result<Option<String>> {
        let entry = Entry::new(SERVICE_NAME, key).map_err(map_keyring_error)?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(error) => Err(map_keyring_error(error)),
        }
    }

    /// Deletes a secret with the given key.
    ///
    /// This operation is idempotent: deleting a key that does not exist is
    /// treated as success (the desired postcondition — "no credential stored
    /// for this key" — is already satisfied).
    ///
    /// # Arguments
    ///
    /// * `key` - The key to delete
    ///
    /// # Returns
    ///
    /// * `io::Result<()>` - Success or an IO error
    pub fn delete(&mut self, key: &str) -> io::Result<()> {
        let entry = Entry::new(SERVICE_NAME, key).map_err(map_keyring_error)?;

        match entry.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(map_keyring_error(e)),
        }
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
}

/// OAuth token methods — only available when the `oauth` feature is enabled,
/// which brings in the `anthropic-auth` crate for the [`TokenSet`] type.
///
// TODO: Extract `TokenSet` from `anthropic-auth` into `querymt-utils` (or other place?) so that
// these methods can live behind `secret-store` without pulling in the full
// `anthropic-auth` dependency.  `anthropic-auth` is our crate, so this is a
// straightforward refactor.
#[cfg(feature = "oauth")]
impl SecretStore {
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
    ///
    /// Keyring failures are logged and reported as `None`; use
    /// [`SecretStore::try_get_oauth_tokens`] to observe them.
    pub fn get_oauth_tokens(&self, provider: &str) -> Option<TokenSet> {
        match self.try_get_oauth_tokens(provider) {
            Ok(tokens) => tokens,
            Err(error) => {
                log::warn!("Failed to read OAuth tokens for '{provider}': {error}");
                None
            }
        }
    }

    /// Retrieves OAuth tokens for a provider, distinguishing "no tokens stored"
    /// (`Ok(None)`) from a keyring failure (`Err`), such as a denied keychain
    /// access prompt. Unparseable stored data is treated as absent.
    pub fn try_get_oauth_tokens(&self, provider: &str) -> io::Result<Option<TokenSet>> {
        let Some(tokens_json) = self.try_get(&format!("oauth_{}", provider))? else {
            return Ok(None);
        };
        match serde_json::from_str(&tokens_json) {
            Ok(tokens) => Ok(Some(tokens)),
            Err(error) => {
                log::warn!("Stored OAuth tokens for '{provider}' are unreadable: {error}");
                Ok(None)
            }
        }
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
