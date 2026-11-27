# provider-account-acp Specification

## Purpose

Defines how an ACP client asks the agent for provider account snapshots, how the agent resolves credentials for the query, which statuses it reports, and how results are cached.

## ADDED Requirements

### Requirement: The account method is advertised

The agent SHALL handle the extension method `querymt/auth/account` and SHALL list it under `methods` in `querymt/capabilities` together with the feature flag `features.auth_account: true`.

#### Scenario: Capabilities advertise the account method
- **WHEN** a client calls `querymt/capabilities`
- **THEN** `methods` contains `querymt/auth/account` and `features.auth_account` is `true`

### Requirement: The account method takes a provider filter

The method SHALL accept optional params `provider` (string) and `refresh` (boolean, default `false`). Without `provider` it SHALL answer for every provider that `querymt/auth/status` lists except `mcp:` entries; with `provider` it SHALL answer for that provider only. An unknown `provider` SHALL be an invalid-params error.

#### Scenario: Unknown provider is rejected
- **WHEN** a client calls the method with `provider: "nope"`
- **THEN** the call fails with invalid params and no provider is queried

### Requirement: The response carries one status entry per provider

The response SHALL be `{ "accounts": [ … ] }` with one entry per provider carrying `provider`, `status` (`ok`, `unsupported`, `unauthenticated` or `error`), `account` (the snapshot, only for `ok`), `fetched_at` (Unix seconds, for `ok` and `unsupported`) and `error` (a message, only for `error`).

#### Scenario: Every configured provider gets one entry
- **WHEN** a client calls the method without params on an agent with three configured providers, one of which reports a snapshot, one of which declines, and one of which has no credential
- **THEN** the response lists exactly those three providers with statuses `ok`, `unsupported` and `unauthenticated`, and only the `ok` entry has an `account`

### Requirement: Credentials are resolved as for model listing

For each HTTP provider the agent SHALL resolve the configuration and credential exactly as it does when listing that provider's catalog (preferred auth method, then OAuth with refresh, stored API key, environment variable) and SHALL pass the resulting pruned configuration to the factory's account query.

#### Scenario: Resolved credential reaches the factory
- **WHEN** a provider's credential resolves through the model-listing chain
- **THEN** the factory's account query receives a configuration whose `api_key` is that credential (an OAuth access token when OAuth is the active method)

### Requirement: Providers without a credential or an HTTP factory are reported, not queried

A provider with no usable credential SHALL be reported as `unauthenticated`; an OAuth refresh failure that requires re-login SHALL also be `unauthenticated` with its message in `error`. A provider whose factory is not HTTP-based SHALL be reported as `unsupported` without resolving credentials. No status message SHALL contain the credential, the request path or query, or the response body.

#### Scenario: Missing credential is unauthenticated
- **WHEN** a provider has no stored key, no environment variable and no OAuth tokens
- **THEN** its entry has status `unauthenticated` and no HTTP request is made for it

#### Scenario: Local provider is unsupported
- **WHEN** a configured provider's factory is not HTTP-based
- **THEN** its entry has status `unsupported` and no credential lookup happens for it

### Requirement: Snapshot failures are reported per provider

A factory error (HTTP failure, classified status, parse failure, timeout) SHALL make that provider's entry `error` with a message, and SHALL NOT fail the whole call or affect other providers' entries. Each provider's credential resolution plus account request SHALL be bounded by a 10-second timeout (loading the provider plugin is not counted) and providers SHALL be queried concurrently.

#### Scenario: One failing provider does not hide the others
- **WHEN** two providers are queried and one endpoint answers 503 while the other returns a snapshot
- **THEN** the response has one `error` entry whose message names the HTTP failure and one `ok` entry with the snapshot

#### Scenario: Hanging endpoint times out
- **WHEN** a provider's account endpoint does not answer within 10 seconds
- **THEN** its entry is `error` naming the timeout and the response is returned without waiting longer

### Requirement: Results are cached briefly and invalidated by auth changes

The agent SHALL cache `ok` and `unsupported` entries per provider for 60 seconds and serve them without querying the provider; `fetched_at` SHALL be the time of the underlying fetch. `unauthenticated` and `error` entries SHALL NOT be cached. `refresh: true` SHALL bypass the cache for the requested providers. Any auth mutation (`querymt/auth/complete`, `logout`, `setApiToken`, `clearApiToken`, `setMethod`) SHALL clear the cache.

#### Scenario: Second call within the TTL is served from cache
- **WHEN** a client calls the method twice within 60 seconds for a provider that returned `ok`
- **THEN** the provider's endpoint is queried once and both responses carry the same `fetched_at`

#### Scenario: Refresh bypasses the cache
- **WHEN** a client calls the method with `refresh: true` within the TTL
- **THEN** the provider's endpoint is queried again and `fetched_at` advances

#### Scenario: An auth mutation invalidates the cache
- **WHEN** a provider returned `ok` and the client then changes its credentials through an auth method (for example `querymt/auth/setApiToken` or `logout`) and queries accounts again within the TTL
- **THEN** the provider's account is fetched again instead of served from the cache
