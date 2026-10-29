# Spec Delta

## Purpose

Configures the iroh mesh relay path (which relays to use, how to authenticate to them, and how long admission waits) so organizations can use self-hosted relays and slow networks can still join.

## ADDED Requirements

### Requirement: Default relays unchanged
With no relay configuration, the iroh mesh endpoint SHALL use the n0 default relays and the existing 15 second admission timeout.

#### Scenario: No relay configuration
- **WHEN** `[mesh]` has no `relays` or `admission_timeout`
- **THEN** the endpoint connects to the n0 default relays
- **AND** an invite join gives up after 15 seconds

### Requirement: Custom relay URLs
`[mesh] relays = ["https://..."]` (or the repeatable `--mesh-relay <url>` flag) SHALL replace the default relays with the listed relays. An explicitly empty list `relays = []` SHALL disable relays, so only direct paths are possible; an absent key keeps the defaults.

#### Scenario: Relays disabled
- **WHEN** `relays = []`
- **THEN** the endpoint connects to no relay and mesh status reports relays as disabled

#### Scenario: Self-hosted relay
- **WHEN** `relays = ["https://relay.corp.example"]`
- **THEN** the endpoint uses only `relay.corp.example` as its home relay and advertises it to peers

#### Scenario: Invalid relay URL
- **WHEN** `relays = ["not a url"]`
- **THEN** startup fails with an error naming `mesh.relays` and the invalid value

### Requirement: Relay authentication token
`[mesh] relay_auth_token` (or the `QMT_MESH_RELAY_TOKEN` environment variable) SHALL be sent to the configured relays for access control. The token SHALL be redacted in logs and status output.

#### Scenario: Token accepted
- **WHEN** a self-hosted relay requires a bearer token and the matching `relay_auth_token` is configured
- **THEN** the relay connection succeeds

#### Scenario: Token rejected
- **WHEN** the relay denies the token
- **THEN** mesh status shows the relay as not connected, with the relay's denial reason

### Requirement: Configurable admission timeout
`[mesh] admission_timeout` (a duration such as `"45s"`) SHALL set how long an invite join keeps retrying before failing. Values below 1 second or above 10 minutes SHALL be rejected at startup.

#### Scenario: Longer timeout on slow network
- **WHEN** `admission_timeout = "60s"`
- **THEN** an unreachable inviter is reported after about 60 seconds, not 15

### Requirement: Relay settings apply to all iroh scopes
Relay, relay token, trust and proxy settings SHALL apply to the shared iroh endpoint used by all iroh mesh scopes, including invite hosting and joining.

#### Scenario: Inviter on a custom relay
- **WHEN** the inviter is configured with a self-hosted relay and generates an invite
- **THEN** a joiner configured with the same relay can be admitted
