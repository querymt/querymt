//! Provider account snapshots for the `querymt/auth/account` ACP method.
//!
//! For each configured provider the credential is resolved as for model
//! listing and the factory's account query is run; the outcome is reported as
//! one [`ProviderAccountStatus`]. `ok` and `unsupported` entries are cached
//! briefly; auth mutations clear the cache.

use super::{OAuthService, auth_provider_names, validate_provider_configured};
use crate::session::error::SessionError;
use crate::session::provider_config::{ProviderConfigMode, resolve_provider_config};
use querymt::error::LLMError;
use querymt::plugin::{AccountReport, ProviderAccount};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use typeshare::typeshare;

/// How long `ok` and `unsupported` entries are served from the cache.
const ACCOUNT_CACHE_TTL: Duration = Duration::from_secs(60);
/// Bound on one provider's credential resolution plus account request (plugin
/// loading is not counted).
const ACCOUNT_QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Outcome of one provider's account query.
#[typeshare]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAccountState {
    /// The provider reported a snapshot.
    Ok,
    /// This provider, or its credential, has no account data.
    Unsupported,
    /// No usable credential.
    Unauthenticated,
    /// The query failed; `error` says why.
    Error,
}

/// Account entry for one provider in the `querymt/auth/account` response.
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderAccountStatus {
    pub provider: String,
    pub status: ProviderAccountState,
    /// The snapshot; present only for `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<ProviderAccount>,
    /// Unix seconds of the underlying fetch; present for `ok` and `unsupported`.
    #[typeshare(serialized_as = "Option<number>")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<i64>,
    /// Why the query failed or the credential is unusable. Never contains the
    /// credential, the request path or query, or the response body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ProviderAccountStatus {
    fn new(provider: &str, status: ProviderAccountState) -> Self {
        Self {
            provider: provider.to_string(),
            status,
            account: None,
            fetched_at: None,
            error: None,
        }
    }

    fn ok(provider: &str, account: ProviderAccount) -> Self {
        Self {
            account: Some(account),
            fetched_at: Some(unix_now()),
            ..Self::new(provider, ProviderAccountState::Ok)
        }
    }

    fn unsupported(provider: &str) -> Self {
        Self {
            fetched_at: Some(unix_now()),
            ..Self::new(provider, ProviderAccountState::Unsupported)
        }
    }

    fn unauthenticated(provider: &str, error: Option<String>) -> Self {
        Self {
            error,
            ..Self::new(provider, ProviderAccountState::Unauthenticated)
        }
    }

    fn failed(provider: &str, error: String) -> Self {
        Self {
            error: Some(error),
            ..Self::new(provider, ProviderAccountState::Error)
        }
    }

    fn is_cacheable(&self) -> bool {
        matches!(
            self.status,
            ProviderAccountState::Ok | ProviderAccountState::Unsupported
        )
    }
}

struct CachedAccount {
    stored: Instant,
    status: ProviderAccountStatus,
}

/// Per-provider cache of `ok` and `unsupported` entries, keyed by provider
/// name only: a credential changed outside the auth methods (environment,
/// reloaded config) is picked up on expiry or with `refresh`.
#[derive(Clone, Default)]
pub(super) struct AccountCache(Arc<parking_lot::Mutex<HashMap<String, CachedAccount>>>);

impl AccountCache {
    fn fresh(&self, provider: &str) -> Option<ProviderAccountStatus> {
        let cache = self.0.lock();
        cache
            .get(provider)
            .filter(|entry| entry.stored.elapsed() < ACCOUNT_CACHE_TTL)
            .map(|entry| entry.status.clone())
    }

    fn store(&self, status: &ProviderAccountStatus) {
        self.0.lock().insert(
            status.provider.clone(),
            CachedAccount {
                stored: Instant::now(),
                status: status.clone(),
            },
        );
    }

    pub(super) fn clear(&self) {
        self.0.lock().clear();
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or_default()
}

/// A provider error as shown to clients: response bodies are dropped.
fn client_message(error: &LLMError) -> String {
    match error {
        LLMError::ResponseFormatError { message, .. } => {
            format!("Response Format Error: {message}")
        }
        other => other.to_string(),
    }
}

impl OAuthService {
    /// Account snapshots for every configured provider, or for `provider` only.
    ///
    /// Providers are queried concurrently, each bounded by a 10 s timeout. With
    /// `refresh`, cached entries for the requested providers are ignored.
    ///
    /// # Errors
    ///
    /// Returns a message when `provider` is not a configured provider.
    pub async fn provider_accounts(
        &self,
        provider: Option<&str>,
        refresh: bool,
    ) -> Result<Vec<ProviderAccountStatus>, String> {
        let providers = match provider {
            Some(name) => {
                validate_provider_configured(&self.config, name)?;
                vec![name.to_string()]
            }
            None => auth_provider_names(&self.config.provider.plugin_registry()),
        };
        let queries = providers
            .iter()
            .map(|name| self.provider_account(name, refresh));
        Ok(futures_util::future::join_all(queries).await)
    }

    async fn provider_account(&self, provider: &str, refresh: bool) -> ProviderAccountStatus {
        if !refresh && let Some(hit) = self.account_cache.fresh(provider) {
            return hit;
        }
        // Loading the provider plugin (and compiling a Wasm module) happens
        // before the timeout starts: it is not the provider being slow.
        let binding = match self
            .config
            .provider
            .provider_resolver()
            .resolve(provider)
            .await
        {
            Ok(binding) => binding,
            Err(error) => return ProviderAccountStatus::failed(provider, client_message(&error)),
        };
        if binding.factory.as_http().is_none() {
            return self.remember(ProviderAccountStatus::unsupported(provider));
        }
        let status = match tokio::time::timeout(
            ACCOUNT_QUERY_TIMEOUT,
            self.fetch_account(provider, &binding),
        )
        .await
        {
            Ok(status) => status,
            Err(_) => ProviderAccountStatus::failed(
                provider,
                format!(
                    "Account query timed out after {} s",
                    ACCOUNT_QUERY_TIMEOUT.as_secs()
                ),
            ),
        };
        self.remember(status)
    }

    fn remember(&self, status: ProviderAccountStatus) -> ProviderAccountStatus {
        if status.is_cacheable() {
            self.account_cache.store(&status);
        }
        status
    }

    /// Resolves the credential and runs the factory's account query; bounded
    /// by the caller's timeout.
    async fn fetch_account(
        &self,
        provider: &str,
        binding: &querymt::plugin::host::ProviderBinding,
    ) -> ProviderAccountStatus {
        let resolved = match resolve_provider_config(
            binding,
            self.config.provider.initial_config(),
            ProviderConfigMode::CatalogListing,
        )
        .await
        {
            Ok(resolved) => resolved,
            Err(SessionError::ProviderError(LLMError::AuthError(_))) => {
                return ProviderAccountStatus::unauthenticated(provider, None);
            }
            Err(SessionError::CredentialRefresh {
                message,
                relogin_required: true,
            }) => return ProviderAccountStatus::unauthenticated(provider, Some(message)),
            Err(error) => return ProviderAccountStatus::failed(provider, error.to_string()),
        };
        match binding.factory.account(&resolved.pruned_config_str).await {
            Ok(AccountReport::Account(account)) => ProviderAccountStatus::ok(provider, account),
            Ok(AccountReport::Unsupported) => ProviderAccountStatus::unsupported(provider),
            Err(error) => {
                log::debug!("Account query for provider '{provider}' failed: {error}");
                ProviderAccountStatus::failed(provider, client_message(&error))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_config::AgentConfig;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::model_inventory::ModelInventory;
    use crate::session::sqlite_storage::SqliteStorage;
    use http::{Request, Response};
    use querymt::plugin::host::PluginRegistry;
    use querymt::plugin::{HTTPLLMProviderFactory, LLMProviderFactory, UsageLimit};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Clone, Copy)]
    enum Reply {
        Snapshot,
        Unavailable,
        Hang,
    }

    /// Loopback account endpoint; counts requests.
    struct Endpoint {
        url: String,
        hits: Arc<AtomicUsize>,
    }

    async fn endpoint(reply: Reply) -> Endpoint {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let served = Arc::clone(&hits);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = [0; 4096];
                let _ = socket.read(&mut request).await;
                served.fetch_add(1, Ordering::SeqCst);
                let response: &[u8] = match reply {
                    Reply::Snapshot => {
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                    }
                    Reply::Unavailable => {
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    }
                    Reply::Hang => {
                        tokio::spawn(async move {
                            let _held = socket;
                            std::future::pending::<()>().await;
                        });
                        continue;
                    }
                };
                let _ = socket.write_all(response).await;
            }
        });
        Endpoint {
            url: format!("http://{addr}/usage"),
            hits,
        }
    }

    /// An HTTP provider whose account request targets `url` (or declines when
    /// `url` is `None`) and records the credential it was given.
    struct AccountProvider {
        name: String,
        url: Option<String>,
        api_key_name: Option<String>,
        seen_key: Arc<parking_lot::Mutex<Option<String>>>,
    }

    impl AccountProvider {
        fn new(name: &str, url: Option<&str>) -> Self {
            Self {
                name: name.to_string(),
                url: url.map(str::to_string),
                api_key_name: None,
                seen_key: Arc::default(),
            }
        }
    }

    impl HTTPLLMProviderFactory for AccountProvider {
        fn name(&self) -> &str {
            &self.name
        }

        fn api_key_name(&self) -> Option<String> {
            self.api_key_name.clone()
        }

        fn config_schema(&self) -> String {
            r#"{"type":"object","properties":{"api_key":{"type":"string"}}}"#.to_string()
        }

        fn list_models_request(&self, _cfg: &str) -> Result<Request<Vec<u8>>, LLMError> {
            Err(LLMError::NotImplemented("unused".into()))
        }

        fn parse_list_models(&self, _resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
            Err(LLMError::NotImplemented("unused".into()))
        }

        fn from_config(&self, _cfg: &str) -> Result<Box<dyn querymt::HTTPLLMProvider>, LLMError> {
            Err(LLMError::NotImplemented("unused".into()))
        }

        fn account_request(&self, cfg: &str) -> Result<Option<Request<Vec<u8>>>, LLMError> {
            let cfg: serde_json::Value = serde_json::from_str(cfg)?;
            *self.seen_key.lock() = cfg
                .get("api_key")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let Some(url) = &self.url else {
                return Ok(None);
            };
            Ok(Some(Request::get(url.as_str()).body(Vec::new())?))
        }

        fn parse_account(&self, _resp: Response<Vec<u8>>) -> Result<ProviderAccount, LLMError> {
            let mut account = ProviderAccount::new();
            account.push_limit(UsageLimit::new("weekly", 12.0));
            Ok(account)
        }
    }

    /// A provider that is not HTTP-based; its account query must never run.
    struct LocalProvider {
        account_calls: Arc<AtomicUsize>,
    }

    impl LLMProviderFactory for LocalProvider {
        fn name(&self) -> &str {
            "acct-local"
        }

        fn config_schema(&self) -> String {
            "{}".to_string()
        }

        fn from_config(&self, _cfg: &str) -> Result<Box<dyn querymt::LLMProvider>, LLMError> {
            Err(LLMError::NotImplemented("unused".into()))
        }

        fn list_models<'a>(
            &'a self,
            _cfg: &str,
        ) -> querymt::plugin::Fut<'a, Result<Vec<String>, LLMError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn account<'a>(
            &'a self,
            _cfg: &str,
        ) -> querymt::plugin::Fut<'a, Result<AccountReport, LLMError>> {
            self.account_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(AccountReport::Unsupported) })
        }
    }

    struct Fixture {
        service: OAuthService,
        _dir: TempDir,
    }

    /// An auth service over a registry whose providers are `configured`
    /// (name, optional api_key) entries backed by the `http` and `local`
    /// static factories.
    async fn fixture(
        configured: &[(&str, Option<&str>)],
        http: Vec<AccountProvider>,
        local: Option<LocalProvider>,
    ) -> Fixture {
        let dir = TempDir::new().unwrap();
        let mut toml = String::new();
        for (name, api_key) in configured {
            toml.push_str(&format!(
                "[[providers]]\nname = \"{name}\"\npath = \"static\"\n"
            ));
            if let Some(key) = api_key {
                toml.push_str(&format!("[providers.config]\napi_key = \"{key}\"\n"));
            }
        }
        if toml.is_empty() {
            toml.push_str("providers = []\n");
        }
        let path = dir.path().join("providers.toml");
        std::fs::write(&path, toml).unwrap();
        let registry = PluginRegistry::from_path(&path).unwrap();
        for provider in http {
            registry.register_static_http(Arc::new(provider));
        }
        if let Some(local) = local {
            registry.register_static(Arc::new(local));
        }
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let config: Arc<AgentConfig> = Arc::new(
            AgentConfigBuilder::new(
                Arc::new(registry),
                storage,
                querymt::LLMParams::new().provider("mock").model("mock"),
            )
            .build(),
        );
        let service = OAuthService::new(
            config.clone(),
            ModelInventory::new(config),
            Default::default(),
            Default::default(),
        );
        Fixture { service, _dir: dir }
    }

    fn by_name<'a>(accounts: &'a [ProviderAccountStatus], name: &str) -> &'a ProviderAccountStatus {
        accounts
            .iter()
            .find(|entry| entry.provider == name)
            .unwrap_or_else(|| panic!("no entry for {name}: {accounts:?}"))
    }

    /// A missing credential is `unauthenticated`, unless the platform secret
    /// store itself is unavailable, which surfaces as `error`.
    fn assert_unauthenticated(entry: &ProviderAccountStatus) {
        match entry.status {
            ProviderAccountState::Unauthenticated => {}
            ProviderAccountState::Error
                if entry
                    .error
                    .as_deref()
                    .is_some_and(|e| e.contains("secret store")) =>
            {
                eprintln!("secret store unavailable; unauthenticated mapping not exercised");
            }
            _ => panic!("expected unauthenticated, got {entry:?}"),
        }
        assert!(entry.account.is_none());
    }

    #[tokio::test]
    async fn every_configured_provider_gets_one_entry() {
        let ok = endpoint(Reply::Snapshot).await;
        let fixture = fixture(
            &[
                ("acct-ok", Some("key-ok")),
                ("acct-declines", Some("key-declines")),
                ("acct-nokey", None),
            ],
            vec![
                AccountProvider::new("acct-ok", Some(&ok.url)),
                AccountProvider::new("acct-declines", None),
                AccountProvider::new("acct-nokey", Some(&ok.url)),
            ],
            None,
        )
        .await;

        let accounts = fixture
            .service
            .provider_accounts(None, false)
            .await
            .unwrap();

        let mut names: Vec<_> = accounts.iter().map(|e| e.provider.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["acct-declines", "acct-nokey", "acct-ok"]);
        let snapshot = by_name(&accounts, "acct-ok");
        assert_eq!(snapshot.status, ProviderAccountState::Ok);
        assert_eq!(
            snapshot.account.as_ref().unwrap().limits,
            [UsageLimit::new("weekly", 12.0)]
        );
        assert!(snapshot.fetched_at.is_some());
        let declined = by_name(&accounts, "acct-declines");
        assert_eq!(declined.status, ProviderAccountState::Unsupported);
        assert!(declined.account.is_none() && declined.fetched_at.is_some());
        assert_unauthenticated(by_name(&accounts, "acct-nokey"));
        assert_eq!(ok.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn resolved_credential_reaches_the_factory() {
        let ok = endpoint(Reply::Snapshot).await;
        let provider = AccountProvider::new("acct-ok", Some(&ok.url));
        let seen_key = Arc::clone(&provider.seen_key);
        let fixture = fixture(&[("acct-ok", Some("configured-key"))], vec![provider], None).await;

        fixture
            .service
            .provider_accounts(Some("acct-ok"), false)
            .await
            .unwrap();

        assert_eq!(seen_key.lock().as_deref(), Some("configured-key"));
    }

    #[tokio::test]
    async fn missing_credential_is_unauthenticated() {
        let ok = endpoint(Reply::Snapshot).await;
        let fixture = fixture(
            &[("acct-nokey", None)],
            vec![AccountProvider::new("acct-nokey", Some(&ok.url))],
            None,
        )
        .await;

        let accounts = fixture
            .service
            .provider_accounts(Some("acct-nokey"), false)
            .await
            .unwrap();

        assert_unauthenticated(&accounts[0]);
        assert_eq!(ok.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn local_provider_is_unsupported() {
        let calls = Arc::new(AtomicUsize::new(0));
        let fixture = fixture(
            &[],
            vec![],
            Some(LocalProvider {
                account_calls: Arc::clone(&calls),
            }),
        )
        .await;

        let accounts = fixture
            .service
            .provider_accounts(Some("acct-local"), false)
            .await
            .unwrap();

        assert_eq!(accounts[0].status, ProviderAccountState::Unsupported);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn one_failing_provider_does_not_hide_the_others() {
        let ok = endpoint(Reply::Snapshot).await;
        let down = endpoint(Reply::Unavailable).await;
        let fixture = fixture(
            &[("acct-ok", Some("k1")), ("acct-down", Some("k2"))],
            vec![
                AccountProvider::new("acct-ok", Some(&ok.url)),
                AccountProvider::new("acct-down", Some(&down.url)),
            ],
            None,
        )
        .await;

        let accounts = fixture
            .service
            .provider_accounts(None, false)
            .await
            .unwrap();

        assert_eq!(
            by_name(&accounts, "acct-ok").status,
            ProviderAccountState::Ok
        );
        let failed = by_name(&accounts, "acct-down");
        assert_eq!(failed.status, ProviderAccountState::Error);
        assert!(
            failed.error.as_deref().unwrap().contains("503"),
            "{failed:?}"
        );
        assert!(failed.account.is_none() && failed.fetched_at.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_endpoint_times_out() {
        let hang = endpoint(Reply::Hang).await;
        let fixture = fixture(
            &[("acct-hang", Some("k"))],
            vec![AccountProvider::new("acct-hang", Some(&hang.url))],
            None,
        )
        .await;

        let accounts = fixture
            .service
            .provider_accounts(Some("acct-hang"), false)
            .await
            .unwrap();

        assert_eq!(accounts[0].status, ProviderAccountState::Error);
        assert!(
            accounts[0].error.as_deref().unwrap().contains("timed out"),
            "{:?}",
            accounts[0]
        );
    }

    #[tokio::test]
    async fn second_call_within_the_ttl_is_served_from_cache() {
        let ok = endpoint(Reply::Snapshot).await;
        let fixture = fixture(
            &[("acct-ok", Some("k"))],
            vec![AccountProvider::new("acct-ok", Some(&ok.url))],
            None,
        )
        .await;

        let first = fixture
            .service
            .provider_accounts(None, false)
            .await
            .unwrap();
        let second = fixture
            .service
            .provider_accounts(None, false)
            .await
            .unwrap();

        assert_eq!(ok.hits.load(Ordering::SeqCst), 1);
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn refresh_bypasses_the_cache() {
        let ok = endpoint(Reply::Snapshot).await;
        let fixture = fixture(
            &[("acct-ok", Some("k"))],
            vec![AccountProvider::new("acct-ok", Some(&ok.url))],
            None,
        )
        .await;

        let first = fixture
            .service
            .provider_accounts(None, false)
            .await
            .unwrap();
        // `fetched_at` has one-second resolution.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let refreshed = fixture.service.provider_accounts(None, true).await.unwrap();

        assert_eq!(ok.hits.load(Ordering::SeqCst), 2);
        assert!(refreshed[0].fetched_at > first[0].fetched_at);
    }

    #[tokio::test]
    async fn failures_are_not_cached() {
        let down = endpoint(Reply::Unavailable).await;
        let fixture = fixture(
            &[("acct-down", Some("k"))],
            vec![AccountProvider::new("acct-down", Some(&down.url))],
            None,
        )
        .await;

        fixture
            .service
            .provider_accounts(None, false)
            .await
            .unwrap();
        fixture
            .service
            .provider_accounts(None, false)
            .await
            .unwrap();

        assert_eq!(down.hits.load(Ordering::SeqCst), 2);
    }

    /// Deletes the test-owned secret-store key on drop.
    struct OwnedKey(String);

    impl Drop for OwnedKey {
        fn drop(&mut self) {
            if let Ok(mut store) = crate::auth::SecretStore::new() {
                let _ = store.delete(&self.0);
            }
        }
    }

    #[tokio::test]
    async fn auth_mutation_invalidates_the_cache() {
        let ok = endpoint(Reply::Snapshot).await;
        // Test-owned names, so no real provider's credentials are touched.
        let name = format!("qmt_test_acct_{}", uuid::Uuid::new_v4().as_simple());
        let key = OwnedKey(format!("QMT_TEST_KEY_{name}"));
        let mut provider = AccountProvider::new(&name, Some(&ok.url));
        provider.api_key_name = Some(key.0.clone());
        let fixture = fixture(&[(&name, Some("k"))], vec![provider], None).await;
        fixture
            .service
            .provider_accounts(None, false)
            .await
            .unwrap();

        let stored = fixture
            .service
            .set_api_token(&name, "test-owned-token")
            .await;
        if !stored.success {
            eprintln!("native keyring unavailable; mutation invalidation not exercised");
            return;
        }
        fixture
            .service
            .provider_accounts(None, false)
            .await
            .unwrap();

        assert_eq!(ok.hits.load(Ordering::SeqCst), 2);
        assert!(fixture.service.clear_api_token(&name).await.success);
        drop(key);
    }

    #[tokio::test]
    async fn unknown_provider_is_rejected() {
        let ok = endpoint(Reply::Snapshot).await;
        let fixture = fixture(
            &[("acct-ok", Some("k"))],
            vec![AccountProvider::new("acct-ok", Some(&ok.url))],
            None,
        )
        .await;

        let error = fixture
            .service
            .provider_accounts(Some("nope"), false)
            .await
            .expect_err("unknown provider");

        assert!(error.contains("nope"), "{error}");
        assert_eq!(ok.hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn response_bodies_are_not_shown_to_clients() {
        let error = LLMError::ResponseFormatError {
            message: "bad shape".to_string(),
            raw_response: "{\"email\":\"someone@example.com\"}".to_string(),
        };

        let message = client_message(&error);

        assert!(message.contains("bad shape"));
        assert!(!message.contains("someone@example.com"));
    }
}
