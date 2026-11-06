//! OAuth credential storage for HTTP MCP servers.
//!
//! Credentials live in the same secret store as provider credentials, one
//! entry per server (`mcp-oauth:<server>`), and are bound to the server URL
//! they were issued for: a server renamed onto another URL never receives
//! another server's token.

#[cfg(feature = "oauth")]
use std::sync::Arc;

/// Loopback redirect port used when a server configures none. Part of the
/// redirect URIs registered with authorization servers: keep it stable.
pub const DEFAULT_MCP_OAUTH_REDIRECT_PORT: u16 = 47821;

/// Provider id prefix under which MCP servers appear in `_querymt/auth/*`.
pub const MCP_PROVIDER_PREFIX: &str = "mcp:";

/// `mcp:<server>` provider id.
pub fn provider_id(server: &str) -> String {
    format!("{MCP_PROVIDER_PREFIX}{server}")
}

/// Server name of an `mcp:<server>` provider id.
pub fn server_of_provider(provider: &str) -> Option<&str> {
    provider.strip_prefix(MCP_PROVIDER_PREFIX)
}

/// Loopback redirect URI for an optional configured port.
pub fn redirect_uri(port: Option<u16>) -> String {
    format!(
        "http://127.0.0.1:{}/callback",
        port.unwrap_or(DEFAULT_MCP_OAUTH_REDIRECT_PORT)
    )
}

/// URLs compare equal when they parse to the same normalised form. This binds
/// credentials to a server, so it is deliberately strict: `…/mcp` and `…/mcp/`
/// are different URLs.
pub fn same_url(a: &str, b: &str) -> bool {
    match (url::Url::parse(a), url::Url::parse(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Minimal async key/value surface the credential store needs.
#[async_trait::async_trait]
pub trait SecretBackend: Send + Sync {
    async fn get(&self, key: &str) -> std::io::Result<Option<String>>;
    async fn set(&self, key: &str, value: String) -> std::io::Result<()>;
    async fn delete(&self, key: &str) -> std::io::Result<()>;
}

/// The system keyring shared with provider credentials.
pub struct KeyringBackend;

#[async_trait::async_trait]
impl SecretBackend for KeyringBackend {
    async fn get(&self, key: &str) -> std::io::Result<Option<String>> {
        let key = key.to_owned();
        crate::auth::SecretStore::new_async()
            .await?
            .run(move |s| s.try_get(&key))
            .await
    }

    async fn set(&self, key: &str, value: String) -> std::io::Result<()> {
        let key = key.to_owned();
        crate::auth::SecretStore::new_async()
            .await?
            .run(move |s| s.set(key, value))
            .await
    }

    async fn delete(&self, key: &str) -> std::io::Result<()> {
        let key = key.to_owned();
        crate::auth::SecretStore::new_async()
            .await?
            .run(move |s| s.delete(&key))
            .await
    }
}

#[cfg(feature = "oauth")]
pub use store::*;

#[cfg(feature = "oauth")]
mod store {
    use super::*;
    use rmcp::transport::{AuthError, CredentialStore, StoredCredentials};
    use serde::{Deserialize, Serialize};

    /// Secret-store key of a server's credentials.
    pub fn credential_key(server: &str) -> String {
        format!("mcp-oauth:{server}")
    }

    #[derive(Serialize, Deserialize)]
    struct Stored {
        server_url: String,
        redirect_uri: String,
        credentials: StoredCredentials,
    }

    /// rmcp [`CredentialStore`] backed by the secret store.
    #[derive(Clone)]
    pub struct SecretStoreCredentialStore {
        server_name: String,
        server_url: String,
        redirect_uri: String,
        backend: Arc<dyn SecretBackend>,
        /// Saves keep the stored `redirect_uri` (see [`Self::for_connection`]).
        keep_stored_redirect: bool,
    }

    // Never prints tokens (and neither does `StoredCredentials`' own Debug).
    impl std::fmt::Debug for SecretStoreCredentialStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SecretStoreCredentialStore")
                .field("server_name", &self.server_name)
                .field("server_url", &self.server_url)
                .finish_non_exhaustive()
        }
    }

    fn store_err(e: impl std::fmt::Display) -> AuthError {
        AuthError::CredentialStoreError(e.to_string())
    }

    impl SecretStoreCredentialStore {
        pub fn new(
            server_name: impl Into<String>,
            server_url: impl Into<String>,
            redirect_uri: impl Into<String>,
        ) -> Self {
            Self::with_backend(
                server_name,
                server_url,
                redirect_uri,
                Arc::new(KeyringBackend),
            )
        }

        pub fn with_backend(
            server_name: impl Into<String>,
            server_url: impl Into<String>,
            redirect_uri: impl Into<String>,
            backend: Arc<dyn SecretBackend>,
        ) -> Self {
            Self {
                server_name: server_name.into(),
                server_url: server_url.into(),
                redirect_uri: redirect_uri.into(),
                backend,
                keep_stored_redirect: false,
            }
        }

        /// Store used by a live connection. Token refreshes save through it,
        /// and must not rewrite the redirect URI the dynamically registered
        /// client was registered with: only a login (which registers the
        /// client again when the URI changed) may change it.
        #[must_use]
        pub fn for_connection(mut self) -> Self {
            self.keep_stored_redirect = true;
            self
        }

        fn key(&self) -> String {
            credential_key(&self.server_name)
        }

        async fn read(&self) -> Result<Option<Stored>, AuthError> {
            let Some(raw) = self.backend.get(&self.key()).await.map_err(store_err)? else {
                return Ok(None);
            };
            // An unreadable entry is treated as absent rather than as a hard
            // failure, so the user can simply log in again.
            // The serde error is deliberately not logged: it can echo the
            // offending value, which here may be a token.
            Ok(serde_json::from_str(&raw)
                .inspect_err(|_| {
                    log::warn!(
                        "ignoring unreadable MCP credentials for `{}`",
                        self.server_name
                    )
                })
                .ok())
        }

        /// Redirect URI the stored client registration was made with, if
        /// credentials exist for this server URL.
        pub async fn stored_redirect_uri(&self) -> Result<Option<String>, AuthError> {
            Ok(self
                .read()
                .await?
                .filter(|s| same_url(&s.server_url, &self.server_url))
                .map(|s| s.redirect_uri))
        }
    }

    #[async_trait::async_trait]
    impl CredentialStore for SecretStoreCredentialStore {
        async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
            Ok(self
                .read()
                .await?
                .filter(|s| same_url(&s.server_url, &self.server_url))
                .map(|s| s.credentials))
        }

        async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
            let redirect_uri = if self.keep_stored_redirect {
                self.stored_redirect_uri()
                    .await?
                    .unwrap_or_else(|| self.redirect_uri.clone())
            } else {
                self.redirect_uri.clone()
            };
            let value = serde_json::to_string(&Stored {
                server_url: self.server_url.clone(),
                redirect_uri,
                credentials,
            })
            .map_err(store_err)?;
            self.backend
                .set(&self.key(), value)
                .await
                .map_err(store_err)
        }

        async fn clear(&self) -> Result<(), AuthError> {
            self.backend.delete(&self.key()).await.map_err(store_err)
        }

        async fn on_refresh_token_rejected(
            &self,
            _credentials: &StoredCredentials,
        ) -> Result<(), AuthError> {
            self.clear().await
        }
    }

    #[cfg(test)]
    pub(crate) mod test_support {
        use super::*;
        use parking_lot::Mutex;
        use std::collections::HashMap;

        /// In-memory [`SecretBackend`] for tests (never touches the keyring).
        #[derive(Default)]
        pub struct MemoryBackend(pub Mutex<HashMap<String, String>>);

        #[async_trait::async_trait]
        impl SecretBackend for MemoryBackend {
            async fn get(&self, key: &str) -> std::io::Result<Option<String>> {
                Ok(self.0.lock().get(key).cloned())
            }
            async fn set(&self, key: &str, value: String) -> std::io::Result<()> {
                self.0.lock().insert(key.to_owned(), value);
                Ok(())
            }
            async fn delete(&self, key: &str) -> std::io::Result<()> {
                self.0.lock().remove(key);
                Ok(())
            }
        }

        pub fn creds(access: &str) -> StoredCredentials {
            let token: rmcp::transport::auth::OAuthTokenResponse = serde_json::from_value(
                serde_json::json!({"access_token": access, "token_type": "bearer",
                                   "refresh_token": "refresh-secret"}),
            )
            .unwrap();
            StoredCredentials::new("client-1".into(), Some(token), vec![], Some(0))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::test_support::*;
        use super::*;

        fn store(backend: &Arc<MemoryBackend>, url: &str) -> SecretStoreCredentialStore {
            SecretStoreCredentialStore::with_backend(
                "linear",
                url,
                redirect_uri(None),
                backend.clone(),
            )
        }

        #[tokio::test]
        async fn connection_saves_keep_the_registered_redirect_uri() {
            let backend = Arc::new(MemoryBackend::default());
            let url = "https://mcp.example/mcp";
            // Logged in (and registered) with the default redirect URI.
            store(&backend, url).save(creds("a")).await.unwrap();
            // Config now says another port; a refresh through the connection
            // store must not rewrite what the client was registered with.
            let moved = SecretStoreCredentialStore::with_backend(
                "linear",
                url,
                redirect_uri(Some(1234)),
                backend.clone(),
            );
            moved
                .clone()
                .for_connection()
                .save(creds("b"))
                .await
                .unwrap();
            assert_eq!(
                moved.stored_redirect_uri().await.unwrap().as_deref(),
                Some(redirect_uri(None).as_str())
            );
            // A login save does record the new URI.
            moved.save(creds("c")).await.unwrap();
            assert_eq!(
                moved.stored_redirect_uri().await.unwrap().as_deref(),
                Some(redirect_uri(Some(1234)).as_str())
            );
        }

        #[tokio::test]
        async fn save_then_load_round_trips() {
            let backend = Arc::new(MemoryBackend::default());
            let s = store(&backend, "https://mcp.example/mcp");
            assert!(s.load().await.unwrap().is_none());
            s.save(creds("a1")).await.unwrap();
            let loaded = s.load().await.unwrap().expect("credentials");
            assert_eq!(loaded.client_id, "client-1");
            assert!(backend.0.lock().contains_key("mcp-oauth:linear"));
            assert_eq!(
                s.stored_redirect_uri().await.unwrap().as_deref(),
                Some("http://127.0.0.1:47821/callback")
            );
        }

        #[tokio::test]
        async fn clear_and_refresh_rejection_delete_the_entry() {
            let backend = Arc::new(MemoryBackend::default());
            let s = store(&backend, "https://mcp.example/mcp");
            s.save(creds("a1")).await.unwrap();
            s.clear().await.unwrap();
            assert!(s.load().await.unwrap().is_none());
            s.save(creds("a1")).await.unwrap();
            s.on_refresh_token_rejected(&creds("a1")).await.unwrap();
            assert!(s.load().await.unwrap().is_none());
            assert!(backend.0.lock().is_empty());
        }

        #[tokio::test]
        async fn credentials_are_not_returned_for_a_different_url() {
            let backend = Arc::new(MemoryBackend::default());
            store(&backend, "https://mcp.example/mcp")
                .save(creds("a1"))
                .await
                .unwrap();
            let other = store(&backend, "https://evil.example/mcp");
            assert!(other.load().await.unwrap().is_none());
            assert!(other.stored_redirect_uri().await.unwrap().is_none());
            // Equivalent spellings of the same URL still match.
            let same = store(&backend, "HTTPS://MCP.example:443/mcp");
            assert!(same.load().await.unwrap().is_some());
        }

        #[tokio::test]
        async fn unreadable_entry_is_treated_as_absent() {
            let backend = Arc::new(MemoryBackend::default());
            backend
                .0
                .lock()
                .insert("mcp-oauth:linear".into(), "{not json".into());
            assert!(
                store(&backend, "https://mcp.example/mcp")
                    .load()
                    .await
                    .unwrap()
                    .is_none()
            );
        }

        #[test]
        fn debug_never_prints_tokens() {
            let s = SecretStoreCredentialStore::new("linear", "https://mcp.example/mcp", "r");
            assert!(!format!("{s:?}").contains("secret"));
            let shown = format!("{:?}", creds("access-secret"));
            assert!(!shown.contains("access-secret") && !shown.contains("refresh-secret"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_and_redirects() {
        assert_eq!(provider_id("linear"), "mcp:linear");
        assert_eq!(server_of_provider("mcp:linear"), Some("linear"));
        assert_eq!(server_of_provider("anthropic"), None);
        assert_eq!(redirect_uri(None), "http://127.0.0.1:47821/callback");
        assert_eq!(redirect_uri(Some(5000)), "http://127.0.0.1:5000/callback");
        assert!(same_url("https://a.test/x", "https://a.test:443/x"));
        assert!(!same_url("https://a.test/x", "https://b.test/x"));
    }
}
