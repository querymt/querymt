# provider-account-snapshot Specification

## Purpose

Defines the account snapshot a provider plugin can report for the credential it runs under (plan, usage limits, credits), how a host obtains it from static, Wasm and native factories, and how providers without such data decline.

## ADDED Requirements

### Requirement: A snapshot has one shape for every provider

A provider account snapshot SHALL consist of an optional `plan` name, a `limits` list of usage limits and an optional `credits` balance. A credit balance SHALL carry an optional decimal-string `balance`, an optional ISO 4217 `currency` and a boolean `unlimited`. The snapshot SHALL serialize to JSON with exactly these snake_case keys and SHALL round-trip through the Extism plugin boundary without loss.

#### Scenario: Snapshot round-trips across the plugin boundary
- **WHEN** a snapshot with a plan, two limits (one scoped, one with `resets_at` and `window_seconds`, one with `used_percent` 120) and a credit balance is serialized by a plugin and deserialized by the host
- **THEN** the host sees an equal snapshot, including the absent fields as absent and the 120 percent unchanged

### Requirement: A usage limit is a percentage with window, reset and scope

Each usage limit SHALL carry a provider-stable `id`, an optional human `label`, an optional `scope` (the model or product the limit applies to; absent means all usage), `used_percent` as a non-negative number that MAY exceed 100, an optional `window_seconds`, an optional `resets_at` in Unix seconds (UTC) and a boolean `reached`. `reached` SHALL be the provider's own blocking flag when it reports one and otherwise `used_percent >= 100`.

#### Scenario: Reached is derived when the provider has no flag
- **WHEN** a parser maps a provider limit at 100 percent from a body that carries no blocking flag
- **THEN** the limit has `reached` true, while a limit at 99 percent from the same body has `reached` false

### Requirement: Factories answer with a snapshot or Unsupported

A provider factory SHALL answer an account query for a resolved configuration with either a snapshot or `Unsupported`. A factory that does not implement the contract SHALL answer `Unsupported` without error. An HTTP factory SHALL answer by either declining or building one HTTP request from the resolved configuration; the host SHALL perform the request and give the full response back to the factory for parsing.

#### Scenario: Default factory reports unsupported
- **WHEN** the account query is sent to a factory that implements neither the request builder nor the generic operation
- **THEN** the result is `Unsupported` and no HTTP request is issued

#### Scenario: HTTP factory snapshot is fetched by the host
- **WHEN** an HTTP factory builds an account request for a resolved configuration and the endpoint answers 200 with a body the factory can parse
- **THEN** the host returns the parsed snapshot and the factory's parser received the full response body and headers

### Requirement: Account endpoint failures are errors, not empty snapshots

A non-success HTTP status from the account endpoint SHALL become the same classified error that model listing produces for that status. A response the factory cannot parse SHALL become a parse error. Neither case SHALL be reported as `Unsupported` or as a partial snapshot.

#### Scenario: Unauthorized account endpoint is classified
- **WHEN** the account endpoint answers 401
- **THEN** the result is the classified authentication error, not a snapshot and not `Unsupported`

#### Scenario: Malformed account body is a parse error
- **WHEN** the account endpoint answers 200 with a body that is not a JSON object
- **THEN** the result is an error naming the parse failure and no partial snapshot is returned

### Requirement: Wasm plugins export the account contract optionally

A Wasm (Extism) HTTP plugin built with the plugin macro SHALL export `account_request` (resolved config in, optional serialized HTTP request out) and `parse_account_response` (serialized HTTP response in, snapshot out). A host SHALL treat a plugin without the `account_request` export as answering `Unsupported` and SHALL NOT fail to load or use such a plugin for chat or model listing. Plugin errors from both exports SHALL keep their structured error payload.

#### Scenario: Older plugin without the exports still works
- **WHEN** the host loads a plugin that exports model listing but not `account_request`
- **THEN** chat and model listing behave as before and the account query answers `Unsupported`

#### Scenario: Plugin decline crosses the boundary as unsupported
- **WHEN** a plugin's `account_request` returns no request for the given configuration
- **THEN** the host answers `Unsupported` without performing any HTTP request

### Requirement: Wasm account requests go only to allowed or declared hosts

For a Wasm plugin the host SHALL issue the request built by `account_request` only when its host is one of the allowed hosts the plugin was loaded with or one of the account hosts the plugin declares; an empty allowed-hosts list SHALL allow every host, as for plugin-initiated HTTP. A request to any other host SHALL NOT be issued and SHALL fail with an invalid-request error naming the host and the `allowed_hosts` configuration key.

#### Scenario: Account request to a host outside the allow-list is refused
- **WHEN** a Wasm plugin loaded with allowed hosts `api.example.com` that declares no account hosts builds an account request for `other.example.net`
- **THEN** no HTTP request is issued and the result is an invalid-request error naming `other.example.net` and `allowed_hosts`

#### Scenario: Configured allowed host lets the account request through
- **WHEN** the same plugin is loaded with `allowed_hosts` containing `other.example.net` and builds the same request
- **THEN** the request is issued and its response is handed to the plugin's parser

### Requirement: Plugins declare account hosts once at load

A Wasm plugin MAY declare the hosts its account requests target through the optional `account_hosts` export; a plugin without the export declares none. The host SHALL read the declaration once when it loads the plugin, SHALL use it for account requests only, and SHALL NOT add it to the hosts the plugin itself may reach. Declared hosts SHALL apply whether or not a `base_url` is configured.

#### Scenario: Declared account host needs no configuration
- **WHEN** a Wasm plugin whose base-URL host is `api.x.ai` declares the account host `cli-chat-proxy.grok.com`, is loaded with no `allowed_hosts` setting, and builds an account request for that host
- **THEN** the request is issued and its response is handed to the plugin's parser

#### Scenario: Declared account host stays out of the plugin's own reach
- **WHEN** the same plugin is loaded
- **THEN** the allowed hosts for plugin-initiated HTTP are `api.x.ai` only

### Requirement: Parsers tolerate absent optional fields

A provider's parser SHALL treat every mapped field as optional: a field that is absent or null omits the plan, limit, limit attribute or credit it would have produced, and a limit without a percentage is omitted. A mapped field of the wrong JSON type SHALL be a parse error. A reset time that is not a UTC RFC 3339 timestamp SHALL leave its limit without `resets_at`.

#### Scenario: Codex body without optional sections still parses
- **WHEN** the usage endpoint returns only `rate_limit.primary_window.used_percent` 40, with no plan, window length, reset time, additional limits or credits
- **THEN** the snapshot has one limit `primary` at 40 percent with no `window_seconds` and no `resets_at`, no plan and no credits

#### Scenario: Unparseable reset time leaves the limit without a reset
- **WHEN** the Claude usage endpoint returns `five_hour` at 24 percent with `resets_at` `2026-11-25T22:09:59+02:00`
- **THEN** the snapshot has limit `five_hour` at 24 percent with `window_seconds` 18000 and no `resets_at`

### Requirement: Codex requests the ChatGPT usage endpoint

The `codex` provider SHALL build its account request against the ChatGPT backend usage endpoint derived from its configured base URL (`../wham/usage` relative to the Codex API base), authenticated with the resolved OAuth token and the `ChatGPT-Account-ID` claim of that token. A token without that claim SHALL fail request building.

#### Scenario: Codex token without an account claim fails before any request
- **WHEN** the resolved configuration's token has no `chatgpt_account_id` claim
- **THEN** building the account request fails with an invalid-request error and no HTTP request is issued

#### Scenario: Codex custom base URL keeps the usage path
- **WHEN** the configuration's base URL is `https://proxy.example/backend-api/codex/`
- **THEN** the account request targets `https://proxy.example/backend-api/wham/usage` with the bearer and account headers

### Requirement: Codex parses the usage body

Codex SHALL parse `plan_type` as the plan; `rate_limit.primary_window` and `rate_limit.secondary_window` as limits `primary` and `secondary` (`window_seconds` from `limit_window_seconds`, `resets_at` from `reset_at`, `reached` from `limit_reached` for the primary window); each `additional_rate_limits[]` entry's windows as limits `<metered_feature>` and `<metered_feature>:secondary` scoped to `limit_name`; and `credits` as a balance without currency.

#### Scenario: Codex usage body becomes a snapshot
- **WHEN** the usage endpoint returns `plan_type: "pro"`, a primary window at 99 percent of a 604800-second window resetting at a given epoch second, no secondary window, one additional limit for `codex_other` named after a model with a primary window at 70 percent, and credits with balance `"12701.63"`
- **THEN** the snapshot has plan `pro`, limits `primary` (99, 604800, that epoch second, not reached) and `codex_other` (70, scoped to that model name), no `secondary` limit, and credits balance `12701.63` not unlimited

### Requirement: xAI reports an account only for xAI OAuth tokens

The `xai` provider SHALL answer `Unsupported` unless its resolved credential is an xAI OAuth access token, recognised as a JWT whose issuer claim is `https://auth.x.ai` and which carries a subject claim; the `auth_type` setting SHALL NOT be required. For such a token it SHALL request `/v1/billing?format=credits` on `cli-chat-proxy.grok.com` with the token as bearer, the Grok CLI token header and the user id taken from the subject claim, and it SHALL declare that host as its only account host.

#### Scenario: xAI API key is unsupported
- **WHEN** the credential is an `xai-…` API key
- **THEN** the account query answers `Unsupported` without an HTTP request

#### Scenario: xAI ignores a token from another issuer
- **WHEN** the credential is a JWT with a subject claim whose issuer is not `https://auth.x.ai`
- **THEN** the account query answers `Unsupported` without an HTTP request

#### Scenario: xAI OAuth request carries the Grok CLI headers
- **WHEN** the credential is a JWT issued by `https://auth.x.ai` whose subject claim is a user id, and no `auth_type` is configured
- **THEN** the account request targets the Grok CLI proxy credits endpoint with the bearer token, the Grok CLI token header and that user id

#### Scenario: xAI declares the Grok CLI proxy as its account host
- **WHEN** the xai factory's account hosts are read
- **THEN** they are exactly `cli-chat-proxy.grok.com`

### Requirement: xAI parses the credits body

xAI SHALL parse `config.creditUsagePercent` and `config.currentPeriod` as limit `weekly` (`used_percent`, `window_seconds` = end − start, `resets_at` = end), each `config.productUsage[]` entry as a limit `weekly:<product>` scoped to that product, and `config.prepaidBalance.val` (cents) as a USD balance. The plan SHALL be absent.

#### Scenario: Grok credits body becomes a snapshot
- **WHEN** the billing endpoint returns a weekly period from a start to an end timestamp, `creditUsagePercent` 22, product usage `Api` 15 and `GrokChat` 7, and prepaid balance 0 cents
- **THEN** the snapshot has limit `weekly` at 22 percent with `window_seconds` 604800 and `resets_at` equal to the end timestamp, limits `weekly:Api` (15, scope `Api`) and `weekly:GrokChat` (7, scope `GrokChat`), credits `0.00` USD, and no plan

### Requirement: Anthropic reports an account only for OAuth credentials

The `anthropic` provider SHALL answer `Unsupported` unless its credential is detected as an OAuth token. With OAuth it SHALL request `/api/oauth/usage` on its base URL with the resolved token as bearer, the OAuth beta header and the API version header used for chat.

#### Scenario: Anthropic API key is unsupported
- **WHEN** the credential is an `sk-ant-api…` key
- **THEN** the account query answers `Unsupported` without an HTTP request

#### Scenario: Anthropic OAuth request carries the OAuth headers
- **WHEN** the credential is an `sk-ant-oat…` token
- **THEN** the account request targets `/api/oauth/usage` on the base URL with the bearer token, the OAuth beta header and the API version header

### Requirement: Anthropic parses the usage body

Anthropic SHALL parse `five_hour` and `seven_day` as limits with those ids (`used_percent` from `utilization`, `window_seconds` 18000 and 604800, `resets_at` from `resets_at`, `reached` when `locked_reason` is set or utilization is at least 100), each `limits[]` entry with a model scope as a limit with `id` = `kind` and `scope` = the model display name, and `spend.balance` as a balance when present. The plan SHALL be absent.

#### Scenario: Claude usage body becomes a snapshot
- **WHEN** the usage endpoint returns `five_hour` at 24 percent and `seven_day` at 40 percent with RFC 3339 reset times, a `limits[]` entry `weekly_scoped` at 10 percent scoped to model display name `Fable`, and no spend balance
- **THEN** the snapshot has limits `five_hour` (24, 18000, the converted epoch second), `seven_day` (40, 604800, the converted epoch second), and `weekly_scoped` (10, scope `Fable`), none reached, no credits and no plan
