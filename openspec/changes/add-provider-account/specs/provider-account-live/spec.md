# provider-account-live Specification

## Purpose

Defines how the account snapshot of a provider is kept current from chat response headers between on-demand fetches, how partial header-derived snapshots merge into cached ones, and how clients are notified.

## ADDED Requirements

### Requirement: HTTP providers can derive a partial snapshot from response headers

An HTTP provider MAY implement a stateless hook that maps a chat response's headers to a partial account snapshot; the default SHALL return nothing. The host SHALL call the hook once per chat response, for both buffered and streamed responses, before parsing the body, and SHALL NOT issue any additional request for it. A Wasm plugin without the `account_from_headers` export SHALL be treated as returning nothing, and the host SHALL NOT call the export for such a plugin.

#### Scenario: Streamed chat response headers reach the hook
- **WHEN** a streamed chat response arrives with usage headers
- **THEN** the provider's hook receives those headers before the first stream chunk is parsed and the stream is delivered unchanged

#### Scenario: Provider without the hook costs nothing
- **WHEN** a chat response arrives for a provider that does not implement the hook
- **THEN** no snapshot is produced, no observer call happens and the chat result is unchanged

### Requirement: Codex maps its response headers

The `codex` provider SHALL map the `x-codex-primary-*` and `x-codex-secondary-*` header families (`used-percent`, `window-minutes` converted to seconds, `reset-at`) to limits `primary` and `secondary`, any other `x-<limit>-primary-used-percent` family to a limit with that id, and `x-codex-credits-*` to a credit balance.

#### Scenario: Codex headers become a partial snapshot
- **WHEN** a Codex response carries `x-codex-primary-used-percent: 99`, `x-codex-primary-window-minutes: 10080`, `x-codex-primary-reset-at: 1795798467` and no secondary family
- **THEN** the hook returns a snapshot with one limit `primary` (99, 604800, 1795798467), no `secondary`, no plan

### Requirement: Anthropic maps its unified OAuth headers only

The `anthropic` provider SHALL, only when its credential is an OAuth token, map `anthropic-ratelimit-unified-5h-utilization` to `five_hour`, `anthropic-ratelimit-unified-7d-utilization` to `seven_day`, the unified reset header to `resets_at` of the window it names, and a `rejected` unified status to `reached`. Anthropic API-key rate-limit headers (`anthropic-ratelimit-requests-*`, `-input-tokens-*`, `-output-tokens-*`) SHALL be ignored.

#### Scenario: Anthropic OAuth headers become a partial snapshot
- **WHEN** an Anthropic response for an OAuth credential carries `anthropic-ratelimit-unified-5h-utilization: 24` and `anthropic-ratelimit-unified-7d-utilization: 40`
- **THEN** the hook returns a snapshot with limits `five_hour` (24) and `seven_day` (40), neither reached

#### Scenario: Anthropic API-key headers produce nothing
- **WHEN** an Anthropic response for an API-key credential carries only `anthropic-ratelimit-requests-*` and `anthropic-ratelimit-input-tokens-*` headers
- **THEN** the hook returns nothing

### Requirement: Header snapshots merge into the cached entry by limit id

A header-derived snapshot SHALL be merged into the provider's cached account entry: limits with a matching `id` are replaced, new ids are appended, other limits are kept; `plan` and `credits` are replaced only when the partial snapshot carries them. When no `ok` entry is cached, the partial snapshot SHALL become the `ok` entry. The entry's `fetched_at` SHALL be the merge time and the cache TTL SHALL restart from it.

#### Scenario: Partial update keeps the fetched plan and scoped limits
- **WHEN** the cache holds an `ok` Codex entry with plan `pro`, limits `primary` and `codex_other`, and credits, and a response header snapshot with only `primary` at a new percentage is merged
- **THEN** the entry keeps plan `pro`, `codex_other` and the credits, and `primary` carries the new percentage

#### Scenario: Header snapshot fills an empty cache
- **WHEN** no entry is cached for a provider and a header snapshot is merged
- **THEN** the next `querymt/auth/account` call returns that snapshot as `ok` without querying the provider

### Requirement: Clients are notified of merged snapshots

After a merge the agent SHALL broadcast the extension notification `querymt/auth/accountChanged` carrying `provider`, the merged `account` and `fetched_at`, to every connected client, at most once per provider per 2 seconds.

#### Scenario: A turn with several responses yields one notification
- **WHEN** a Codex turn produces three responses with usage headers within one second
- **THEN** connected clients receive one `querymt/auth/accountChanged` for `codex` carrying the last merged snapshot

### Requirement: A completed turn makes a header-less provider's entry stale

When a run ends, the cached entry of the run's provider SHALL be marked stale so that the next `querymt/auth/account` call for that provider bypasses the cache TTL. The agent SHALL NOT issue any provider request on its own at turn end.

#### Scenario: Next account call after an xAI turn is fresh
- **WHEN** an xAI OAuth entry was fetched 10 seconds ago, a turn on xAI completes, and the client calls `querymt/auth/account { provider: "xai" }`
- **THEN** the provider's credits endpoint is queried again and `fetched_at` advances, while no request was made between the turn end and the call
