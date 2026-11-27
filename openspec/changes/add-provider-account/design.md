# Design: Provider account snapshots

## Goals and boundaries

One additive contract, the same shape for every provider, and no new code path for credentials, HTTP or plugin loading. Everything below builds on the model-listing path, which already solves "run a credentialed, provider-specific GET through the host for static, Wasm and native factories".

Boundaries: one request per snapshot, pull only (no header or stream capture; live updates are a follow-up change), no persistence, no new config keys.

## D1: The snapshot is an *account*, usage is one field of it

`crates/querymt/src/plugin/account.rs` (new, re-exported from `querymt::plugin`):

```rust
/// What a factory answers to an account query. In-process only (never serialized);
/// the Extism boundary carries `Option<SerializableHttpRequest>` and `ProviderAccount`.
#[derive(Debug, Clone, PartialEq)]
pub enum AccountReport {
    /// This provider, or this credential, has no account data (most inference API keys, local models).
    Unsupported,
    Account(ProviderAccount),
}

#[cfg_attr(feature = "type-generation", typeshare::typeshare)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[non_exhaustive]
pub struct ProviderAccount {
    /// Plan or tier as the provider names it ("pro", "claude_max"); None when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Rolling or calendar limits; empty for providers that only report credits.
    #[serde(default)]
    pub limits: Vec<UsageLimit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<CreditBalance>,
}

#[cfg_attr(feature = "type-generation", typeshare::typeshare)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UsageLimit {
    /// Provider-stable key: "primary", "secondary", "five_hour", "seven_day", "weekly", a metered feature id.
    pub id: String,
    /// Human label when the provider gives one ("Weekly", "Api"); clients fall back to `id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Model or product the limit applies to ("Opus", "Api", "gpt-6-astra"); None = all usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Percent of the allowance consumed; may exceed 100.
    pub used_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_seconds: Option<u32>,
    /// Unix seconds (UTC) when the window resets.
    #[cfg_attr(feature = "type-generation", typeshare(serialized_as = "Option<number>"))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    /// The provider reports this limit as blocking requests now.
    pub reached: bool,
}

#[cfg_attr(feature = "type-generation", typeshare::typeshare)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CreditBalance {
    /// Decimal string in the provider's unit ("12701.63", "0.00"); None when the provider only reports has/has-not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
    /// ISO 4217 code when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    pub unlimited: bool,
}
```

Construction: the structs are `#[non_exhaustive]` (as `ChatCall` is) so later fields are not a break for plugin crates, which therefore build them through `#[must_use]` constructors instead of literals: `ProviderAccount::new()` with `with_plan`, `push_limit`, `with_credits`; `UsageLimit::new(id, used_percent)` with `with_label`, `with_scope`, `with_window_seconds`, `with_resets_at`, `with_reached` (unset → derived from `used_percent >= 100.0` at construction); `CreditBalance::new()` with `with_balance`, `with_currency`, `with_unlimited`. Fields stay `pub` for reading. Integer conventions follow the repo: epoch seconds are `i64` with `typeshare(serialized_as = "number")` (`agent-core/src/delegation_update.rs`), durations that fit are `u32`; conversions from provider payloads use `TryFrom`/`saturating_sub`, never `as`.

Why percent and not tokens: Codex, Claude and Grok all report percent used; none reports the token allowance. Why epoch seconds: no date dependency in `querymt`, and Codex already uses it. Claude and Grok return RFC 3339 timestamps in UTC with a `+00:00` offset and fractional seconds (observed: `2026-11-25T22:09:59.989045+00:00` and `2026-11-29T16:16:02.926+00:00`). The shared helper `querymt::plugin::account::parse_rfc3339_utc(&str) -> Option<i64>` wraps `humantime::parse_rfc3339_weak`, which accepts `Z` or `+00:00`, with or without fractional seconds, and rejects every other offset (read in the 2.3.0 and 2.4.0 sources; the lockfile has 2.4.0). `querymt` already depends on `humantime = "2"`; the requirement is raised to `"2.3"`, the lowest version checked. A timestamp the helper rejects yields `None` and the limit is reported without `resets_at`. The helper lives in `querymt` so all plugins share it and wasm32 builds need nothing extra. `reached` is the provider's own flag (`limit_reached`, `locked_reason`), else `used_percent >= 100`.

Typeshare attributes follow `crates/querymt/src/chat/mod.rs`. `AccountReport` is a plain in-process enum; the ACP layer flattens it into the status entry (D5), and it is matched exhaustively there (adding a variant later is a deliberate, visible break for the one in-workspace matcher).

## D2: Factory contract mirrors model listing

`crates/querymt/src/plugin/http.rs HTTPLLMProviderFactory` gains:

```rust
/// Build the request for the account snapshot, or None when this provider or
/// this credential has nothing to report (most inference API keys, local providers).
fn account_request(&self, _cfg: &str) -> Result<Option<Request<Vec<u8>>>, LLMError> { Ok(None) }
fn parse_account(&self, _resp: Response<Vec<u8>>) -> Result<ProviderAccount, LLMError> {
    Err(LLMError::NotImplemented("account snapshot".into()))
}
/// Hosts other than the base-URL host that `account_request` may target.
/// A Wasm host reads it once at load; `ExtismFactory` returns what it read.
fn account_hosts(&self) -> Vec<String> { Vec::new() }
```

`crates/querymt/src/plugin/mod.rs LLMProviderFactory` gains the one method callers use:

```rust
fn account<'a>(&'a self, _cfg: &str) -> Fut<'a, Result<AccountReport, LLMError>> {
    Box::pin(async { Ok(AccountReport::Unsupported) })
}
```

`account_request` keeps `Option<Request>` because it answers a narrower question ("is there anything to fetch?"); only `account` is part of the caller-facing vocabulary, and there `Unsupported` is a named outcome rather than `None`.

- `crates/querymt/src/plugin/adapters.rs HTTPFactoryAdapter::account`: `account_request` → `call_outbound` → `parse_account`, like `list_models`. A non-success status goes through `classify_http_status` (the `handle_http_error!` rule for non-chat paths) so 401/403 map to an auth error and 429/5xx stay retryable.
- `crates/querymt/src/plugin/extism_impl/host/mod.rs`: `ExtismFactory` implements both traits' new methods the way `list_models_request` / `parse_list_models` are implemented (lock plugin, `call_get_error_code`, `decode_plugin_error`), with the `spawn_blocking` wrapping from `ExtismFactory::list_models`. `account_request` returns `Ok(None)` without calling the plugin when `!plug.function_exists("account_request")`, so `account` reports `Unsupported`. Before issuing the request, `ExtismFactory::account` checks its host against the factory's allowed hosts and the plugin's declared account hosts (D4).
- `LLMProviderFactory::account` sits on the generic trait because the runtime holds `dyn LLMProviderFactory` and needs one call that `HTTPFactoryAdapter` and `ExtismFactory` both answer. It is a dispatch point, not a contract for non-HTTP factories: llama-cpp, mrs and izwi inherit `Unsupported`, and the runtime reports them as `unsupported` without calling it (D5).

The config string is the same pruned JSON the factory accepts for `from_config` and `list_models`; it contains `api_key` (already an OAuth access token when that is the active method) and `base_url`. The runtime sets no marker for the auth method: `resolve_provider_config` writes `api_key` and nothing else, and does not set `auth_type`. A plugin therefore recognises an OAuth credential from the token itself.

## D3: Extism exports

`crates/querymt-extism-macros/src/lib.rs impl_extism_http_plugin!` adds:

```rust
#[plugin_fn] pub fn account_request(Json(input): Json<ExtismAccountRequest>) -> FnResult<Json<Option<SerializableHttpRequest>>>
#[plugin_fn] pub fn parse_account_response(Json(input): Json<ExtismAccountParseRequest>) -> FnResult<Json<ProviderAccount>>
#[plugin_fn] pub fn account_hosts() -> FnResult<Json<Vec<String>>>
```

with `ExtismAccountRequest { cfg: Value }` and `ExtismAccountParseRequest { resp: SerializableHttpResponse }` in `crates/querymt/src/plugin/extism_impl/interface.rs`, next to `ExtismListModelsRequest`. `account_hosts` takes no input and returns `HTTPLLMProviderFactory::account_hosts(&$Factory)`, as `api_key_name` does. Errors are encoded with `PluginError::encode`, as every other export. A plugin built before this change simply lacks the exports; the host's `function_exists` probe makes it `Unsupported`. No contract-version constant changes (see proposal Impact for the review flag).

## D4: Plugin implementations

- `crates/providers/codex/src/lib.rs CodexFactory`: `account_request` builds `GET {base_url}/../wham/usage` (base URL `https://chatgpt.com/backend-api/codex/` → `https://chatgpt.com/backend-api/wham/usage`; derive by joining `../wham/usage` so custom base URLs keep working) with `Authorization: Bearer`, `ChatGPT-Account-ID` from `api.rs chatgpt_account_id` and `originator: codex_cli_rs`, as chat. `parse_account`: `plan = plan_type`; limits `primary`/`secondary` from `rate_limit.{primary_window,secondary_window}` (`used_percent`, `limit_window_seconds`, `reset_at`, `reached = rate_limit.limit_reached` for primary), plus, per `additional_rate_limits[]` entry, its own `rate_limit.primary_window` as limit `<metered_feature>` and `secondary_window` as `<metered_feature>:secondary`, both scoped to `limit_name` (the upstream payload, `codex-backend-openapi-models AdditionalRateLimitDetails`, nests windows and has no model slug); credits from `credits.{balance, unlimited}` with currency `None` (ChatGPT credits are a unit, not a currency).
- `crates/providers/xai/src/lib.rs XaiFactory`: `account_request` returns `None` unless `api_key` is an xAI OAuth access token, recognised as a JWT whose payload has `iss` `https://auth.x.ai` and a string `sub`. `auth_type` is not consulted, because the runtime does not set it. The issuer test keeps a JWT from another service, used with a custom `base_url`, from being sent to Grok. `account_hosts` returns `["cli-chat-proxy.grok.com"]`. For an xAI OAuth token: `GET https://cli-chat-proxy.grok.com/v1/billing?format=credits` with `Authorization: Bearer`, `X-XAI-Token-Auth: xai-grok-cli`, `x-userid: <JWT sub>` (decode the unsigned payload like `chatgpt_account_id` does). The host URL is a constant, not derived from `base_url` (`api.x.ai`). The two extra headers are kept as grok-build sends them. In one probe (2026-11-25) the endpoint also answered 200 with the bearer token alone, so the code carries a `TODO` where they are set: consider identifying as QueryMT in `X-XAI-Token-Auth`, or dropping that header and `x-userid`. `parse_account`: one limit `weekly` (`label` from `currentPeriod.type`, `used_percent = creditUsagePercent`, `window_seconds = end - start`, `resets_at = end`), one scoped limit per `productUsage[]` entry (`id = "weekly:" + product`, `scope = product`), credits from `prepaidBalance.val` cents → `"12.34"`, currency `USD`. `plan = None`.
- `crates/providers/anthropic/src/factory.rs`: `account_request` returns `None` unless `detect_auth_type` says OAuth; otherwise `GET {base_url}/api/oauth/usage` with `Authorization: Bearer`, `anthropic-beta: oauth-2025-04-20`, `anthropic-version: 2023-06-01`. `parse_account`: limits `five_hour`, `seven_day` from the top-level objects (`used_percent = utilization`, `window_seconds` 18000 / 604800, `resets_at` parsed from RFC 3339, `reached = locked_reason.is_some() || utilization >= 100`), plus one scoped limit per `limits[]` entry whose `scope.model` is set (`id = kind`, `scope = scope.model.display_name`). `plan = None`. Credits from `spend.balance` only when it is a money object `{amount_minor, currency, exponent}` (the shape of `spend.used`; `balance` was `null` when probed), formatted as a decimal string. The request also carries the `claude-cli` user agent the OAuth model listing sends.

Parser tolerance: each parser reads the body as a JSON object and treats every mapped field as optional. An absent or null field omits the plan, limit, limit attribute or credit it would have produced, and a limit whose percentage is absent is omitted. A body that is not a JSON object, or a mapped field of the wrong JSON type, is `LLMError::ResponseFormatError`, which the runtime reports as that provider's `error` entry. These endpoints are undocumented (proposal Risks), so shape drift has to degrade to fewer fields or one `error` entry.

Allowed hosts for Wasm plugins: `ExtismFactory` already holds the allowed hosts it was loaded with (`allowed_hosts`, built by `build_allowed_hosts` from `[providers.config].allowed_hosts` plus the configured `base_url` host, or else the plugin's default `base_url` host) and already rejects a runtime `base_url` outside them (`validate_runtime_base_url`). Plugin-initiated HTTP is checked in the host functions (`extism_impl/host/functions.rs`), but a request the host runs for a plugin is not, so without a check a plugin could make the host send the user's token to a host chosen at call time. `ExtismFactory::account` therefore issues the request returned by `account_request` only when its host is in `allowed_hosts` or in the plugin's declared account hosts (an empty `allowed_hosts` allows every host, as today). Otherwise it returns `LLMError::InvalidRequest` naming the provider, the host and `[providers.config].allowed_hosts`, without issuing the request.

Declared account hosts: `ExtismFactory::load` reads the optional `account_hosts` export once, on the init plugin where it already reads `name` and `base_url`, stores the result as `account_hosts: Vec<String>` and logs it with the allowed hosts. A plugin without the export declares nothing. The list is used only by the account check. It is not passed to `Manifest::with_allowed_hosts` or to `HostState`, so the plugin's own HTTP reach is unchanged. This is the trust the host already gives the plugin's `base_url` export, whose host is allowed without configuration: it does not protect against a plugin that is itself malicious, and it does stop a request to a host that was neither configured nor declared at load. Declared hosts apply whether or not `base_url` is configured, because the account endpoint does not move with the chat base URL.

Consequences: Codex and Anthropic declare nothing, because their usage endpoints are on the base-URL host (also with a custom `base_url`). xAI declares `cli-chat-proxy.grok.com`, so its account query works under Wasm with no configuration; that host is contacted only for an account request, which xAI builds only for an xAI OAuth token. Statically linked factories are in-tree code and have no allow-list. Model listing and prepared chat requests keep their current behaviour (host-run, unchecked); closing that is outside this change.

## D5: Runtime method and caching

```mermaid
sequenceDiagram
    participant C as ACP client
    participant H as LocalAgentHandle
    participant S as OAuthService
    participant R as resolve_provider_config
    participant F as factory.account
    C->>H: querymt/auth/account {provider?, refresh?}
    H->>S: provider_accounts(filter, refresh)
    loop each configured provider (concurrently)
        S->>S: cache hit and !refresh? return entry
        S->>S: resolve binding (loads the plugin; not timed)
        S->>R: CatalogListing (OAuth refresh / stored key / env), 10 s bound starts
        alt no credential
            R-->>S: Err → status unauthenticated
        else
            S->>F: account(pruned cfg)
            F-->>S: Unsupported → unsupported / Account → ok / Err → error
        end
    end
    S-->>H: Vec<ProviderAccountStatus>
    H-->>C: { accounts: [...] }
```

- Handler in `crates/agent-runtime/src/agent/handle/ext_auth.rs` next to `handle_ext_auth_status`; dispatch entry in `handle/ext.rs`; method and `auth_account: true` in `crates/agent-runtime/src/control/capabilities.rs`.
- Logic in a new `crates/agent-runtime/src/auth/service/account.rs` (a child of `auth/service.rs`, like `service/mcp.rs`, so it can use the service's private fields) as `impl OAuthService` (it already owns `config`, the registry and `invalidate_auth_caches`). Provider set: `auth_provider_names(registry)` filtered like `auth_status`. Credentials: `crate::session::provider_config::resolve_provider_config(&binding, config.provider.initial_config(), ProviderConfigMode::CatalogListing)` exactly as `model_registry.rs fetch_catalog_models`. Non-HTTP factories skip resolution and report `unsupported`.
- Status type (typeshared, in `auth/service.rs` next to `AuthProviderStatus`):

```rust
pub struct ProviderAccountStatus {
    pub provider: String,
    pub status: ProviderAccountState,      // ok | unsupported | unauthenticated | error
    pub account: Option<ProviderAccount>,  // ok only
    #[typeshare(serialized_as = "Option<number>")]
    pub fetched_at: Option<i64>,           // ok and unsupported; unix seconds
    pub error: Option<String>,             // error only
}
```

- Cache: `parking_lot::Mutex<HashMap<String, (Instant, ProviderAccountStatus)>>` on `OAuthService` (short critical sections, never held across an `.await`; the same choice as `ModelInventory`'s caches, while `OAuthService`'s existing `tokio::sync::Mutex` guards awaited flows), TTL 60 s for `ok` and `unsupported`; `unauthenticated` and `error` are never cached. `invalidate_auth_caches` clears it, so login, logout, token changes and method changes take effect on the next call. `refresh: true` ignores the cache for the requested providers. The cache key is the provider name only, so a credential changed outside the ACP auth methods (an environment variable, a reloaded config `api_key`) is picked up when the entry expires or on `refresh: true`.
- Errors: `SessionError::CredentialRefresh` and missing-credential errors map to `unauthenticated`; `LLMError` from the factory maps to `error` with `Display` text and no secrets (request paths, queries and bodies are not echoed; a refused-host error names the host only).

## D6: What stays out of the plugin trait on purpose

A single async `account(cfg)` on `HTTPLLMProviderFactory` that does its own HTTP would let plugins chain requests, but the HTTP factory traits are deliberately sync request builders so one host implementation serves static, Wasm and native loading and so outbound policy stays in the host. Chaining can be added later as `parse_account` returning "done or next request" without changing this wire shape.

## Validation builds

- `querymt`: `--all-features` (CI set).
- Providers: `--no-default-features --features native` for tests; `cargo build -p qmt-{codex,xai,anthropic} --release --target wasm32-wasip1` to prove the new exports compile for Wasm.
- `querymt-agent-runtime`: `cargo t-runtime`; `querymt-agent`: `cargo t-agent --no-run` with `QMT_UI_DIST`.
