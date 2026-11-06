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
        /// Only grants within these scopes are stored or loaded
        /// (see [`Self::restrict_scopes`]).
        allowed_scopes: Option<Vec<String>>,
    }

    // Never prints tokens (and neither does `StoredCredentials`' own Debug).
    impl std::fmt::Debug for SecretStoreCredentialStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SecretStoreCredentialStore")
                .field("server_name", &self.server_name)
                .field("server_url", &self.server_url)
                .field("allowed_scopes", &self.allowed_scopes)
                .finish_non_exhaustive()
        }
    }

    /// Low-cardinality class of an OAuth failure, for the `error.type` span
    /// attribute and metric labels. Never contains server-provided text.
    pub fn auth_error_type(error: &AuthError) -> &'static str {
        let forbidden = |text: &str| text.contains("403");
        match error {
            AuthError::RegistrationFailed(m) if forbidden(m) => "registration_http_403",
            AuthError::RegistrationFailed(_) => "registration",
            AuthError::MetadataError(m) if forbidden(m) => "metadata_http_403",
            AuthError::MetadataError(_)
            | AuthError::NoAuthorizationSupport
            | AuthError::PkceUnsupported
            | AuthError::AuthorizationServerMismatch { .. } => "metadata",
            AuthError::AuthorizationServerMissingIssuer { .. } => "missing_issuer",
            AuthError::TokenExchangeFailed(_) | AuthError::OAuthError(_) => "token_exchange",
            AuthError::CredentialStoreError(m) if m.contains("strict_scopes") => "scope_exceeded",
            AuthError::CredentialStoreError(_) => "credential_store",
            AuthError::HttpError(_) => "http",
            AuthError::UrlError(_) => "url",
            _ => "other",
        }
    }

    /// `User-Agent` of every OAuth request (discovery, registration, token).
    ///
    /// Some authorization servers (IBKR) answer requests without one with
    /// `403`, and rmcp's own client sends none.
    fn user_agent() -> String {
        format!("querymt-agent/{}", env!("QMT_BUILD_VERSION"))
    }

    /// rmcp's default OAuth HTTP client plus a `User-Agent` header.
    ///
    /// Like every custom client it leaves the check of the *resolved*
    /// addresses of discovered hosts to the network; rmcp still checks the
    /// host strings. Server URLs come from the user's own configuration.
    struct UserAgentOAuthClient<C> {
        inner: C,
        user_agent: reqwest::header::HeaderValue,
    }

    impl<C: rmcp::transport::OAuthHttpClient> rmcp::transport::OAuthHttpClient
        for UserAgentOAuthClient<C>
    {
        fn execute(
            &self,
            mut request: rmcp::transport::OAuthHttpRequest,
        ) -> rmcp::transport::OAuthHttpClientFuture<'_> {
            request
                .request
                .headers_mut()
                .entry(reqwest::header::USER_AGENT)
                .or_insert_with(|| self.user_agent.clone());
            self.inner.execute(request)
        }
    }

    /// The HTTP client for every OAuth operation of an MCP server: login
    /// (`AuthorizationManager::new_with_oauth_http_client`) and the
    /// connection's token refreshes.
    pub fn oauth_http_client() -> Result<Arc<dyn rmcp::transport::OAuthHttpClient>, AuthError> {
        let user_agent = reqwest::header::HeaderValue::from_str(&user_agent())
            .map_err(|e| AuthError::InternalError(e.to_string()))?;
        Ok(Arc::new(UserAgentOAuthClient {
            inner: rmcp::transport::auth::default_oauth_http_client()?,
            user_agent,
        }))
    }

    /// Scope always accepted on top of the configured ones: it only asks for
    /// a refresh token.
    const OFFLINE_ACCESS: &str = "offline_access";

    /// Scopes `credentials` carry beyond `allowed`, sorted.
    ///
    /// Collected from the recorded grant, the token response `scope` and, when
    /// the access token is a JWT, its `scope` / `scp` claim. The JWT is only
    /// decoded, not verified: it is the authorization server's own statement
    /// of what the token may do, read to refuse a wider grant.
    pub fn excess_scopes(allowed: &[String], credentials: &StoredCredentials) -> Vec<String> {
        let mut granted: std::collections::BTreeSet<String> =
            credentials.granted_scopes.iter().cloned().collect();
        if let Some(token) = &credentials.token_response
            && let Ok(token) = serde_json::to_value(token)
        {
            if let Some(scope) = token.get("scope") {
                granted.extend(scope_values(scope));
            }
            if let Some(access) = token.get("access_token").and_then(|v| v.as_str()) {
                granted.extend(jwt_scopes(access));
            }
        }
        granted
            .into_iter()
            .filter(|s| s != OFFLINE_ACCESS && !allowed.contains(s))
            .collect()
    }

    /// Scopes in a space-delimited string or an array of strings.
    fn scope_values(value: &serde_json::Value) -> Vec<String> {
        match value {
            serde_json::Value::String(s) => s.split_whitespace().map(str::to_owned).collect(),
            serde_json::Value::Array(items) => items
                .iter()
                .filter_map(|v| v.as_str())
                .flat_map(str::split_whitespace)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// `scope` / `scp` claims of a JWT access token; empty for opaque tokens.
    fn jwt_scopes(token: &str) -> Vec<String> {
        use base64::Engine as _;
        let mut parts = token.split('.');
        let (Some(_), Some(payload), Some(_), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Vec::new();
        };
        let Ok(bytes) =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('='))
        else {
            return Vec::new();
        };
        let Ok(claims) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return Vec::new();
        };
        ["scope", "scp"]
            .iter()
            .filter_map(|claim| claims.get(claim))
            .flat_map(scope_values)
            .collect()
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
                allowed_scopes: None,
            }
        }

        /// Apply the scope policy of a server's `auth` settings: with
        /// `strict_scopes`, see [`Self::restrict_scopes`].
        #[must_use]
        pub fn with_auth_policy(self, auth: &crate::config::McpAuth) -> Self {
            match auth {
                crate::config::McpAuth::OAuth {
                    scopes,
                    strict_scopes: true,
                    ..
                } => self.restrict_scopes(scopes.clone()),
                crate::config::McpAuth::OAuth { .. } => self,
            }
        }

        /// Refuse credentials granting more than `allowed` (plus
        /// `offline_access`): saving them fails, so a wider login or refresh
        /// is never persisted, and stored ones load as absent, so they are
        /// never used to connect.
        #[must_use]
        pub fn restrict_scopes(mut self, allowed: Vec<String>) -> Self {
            self.allowed_scopes = Some(allowed);
            self
        }

        /// Scopes `credentials` carry beyond the allowed ones (empty when
        /// unrestricted).
        fn excess(&self, credentials: &StoredCredentials) -> Vec<String> {
            self.allowed_scopes
                .as_deref()
                .map(|allowed| excess_scopes(allowed, credentials))
                .unwrap_or_default()
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
            let credentials = self
                .read()
                .await?
                .filter(|s| same_url(&s.server_url, &self.server_url))
                .map(|s| s.credentials);
            Ok(credentials.filter(|c| {
                let excess = self.excess(c);
                if !excess.is_empty() {
                    tracing::warn!(
                        server = %self.server_name,
                        excess = ?excess,
                        "ignoring stored MCP credentials: the grant exceeds the configured scopes"
                    );
                }
                excess.is_empty()
            }))
        }

        async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
            let excess = self.excess(&credentials);
            if !excess.is_empty() {
                return Err(store_err(format_args!(
                    "the authorization server granted scopes beyond the configured ones ({}); \
                     not storing them because `strict_scopes` is set",
                    excess.join(", ")
                )));
            }
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

        fn creds_with(scope: Option<&str>, access: &str) -> StoredCredentials {
            let mut body = serde_json::json!({"access_token": access, "token_type": "bearer"});
            if let Some(scope) = scope {
                body["scope"] = scope.into();
            }
            let token: rmcp::transport::auth::OAuthTokenResponse =
                serde_json::from_value(body).unwrap();
            StoredCredentials::new("c".into(), Some(token), vec![], Some(0))
        }

        fn jwt(claims: serde_json::Value) -> String {
            use base64::Engine as _;
            let enc = |v: &serde_json::Value| {
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
            };
            format!(
                "{}.{}.sig",
                enc(&serde_json::json!({"alg": "RS256"})),
                enc(&claims)
            )
        }

        #[test]
        fn excess_scopes_reads_response_scope_and_jwt_claims() {
            let allowed = vec!["mcp.read".to_string()];
            assert!(
                excess_scopes(&allowed, &creds_with(Some("mcp.read offline_access"), "x"))
                    .is_empty()
            );
            assert_eq!(
                excess_scopes(
                    &allowed,
                    &creds_with(Some("mcp.read mcp.orders.submit"), "x")
                ),
                ["mcp.orders.submit"]
            );
            // Opaque token without `scope`: nothing to object to.
            assert!(excess_scopes(&allowed, &creds_with(None, "opaque")).is_empty());
            // The JWT says more than the (absent) response scope.
            let token = jwt(serde_json::json!({"scp": ["mcp.read", "mcp.write"]}));
            assert_eq!(
                excess_scopes(&allowed, &creds_with(None, &token)),
                ["mcp.write"]
            );
            let token = jwt(serde_json::json!({"scope": "mcp.read"}));
            assert!(excess_scopes(&allowed, &creds_with(None, &token)).is_empty());
        }

        #[tokio::test]
        async fn restricted_store_refuses_and_hides_wider_grants() {
            let backend = Arc::new(MemoryBackend::default());
            let url = "https://mcp.example/mcp";
            let wide = creds_with(Some("mcp.read mcp.orders.submit"), "x");
            // Stored earlier without a restriction.
            store(&backend, url).save(wide.clone()).await.unwrap();
            let strict = store(&backend, url).restrict_scopes(vec!["mcp.read".into()]);
            assert!(
                strict.load().await.unwrap().is_none(),
                "not used to connect"
            );
            let err = strict.save(wide).await.unwrap_err().to_string();
            assert!(err.contains("mcp.orders.submit"), "{err}");
            strict
                .save(creds_with(Some("mcp.read"), "y"))
                .await
                .unwrap();
            assert!(strict.load().await.unwrap().is_some());
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
