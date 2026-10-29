# Spec Delta

## Purpose

Defines how qmtcode routes outbound HTTP(S) traffic through forward proxies, so the mesh relay, HTTP clients and model downloads work on proxy-only enterprise networks.

## ADDED Requirements

### Requirement: Explicit proxy configuration
Users SHALL be able to set a forward proxy through `network.proxy.url` or the `--proxy <url>` flag. Supported schemes are `http` and `https`. An explicit proxy SHALL take precedence over proxy environment variables.

#### Scenario: Explicit proxy for the mesh relay
- **WHEN** `[network.proxy] url = "http://proxy.corp:3128"` is set
- **THEN** the mesh relay WebSocket connection and pkarr publishing go through `proxy.corp:3128`

#### Scenario: Explicit proxy overrides environment
- **WHEN** both `network.proxy.url` and `HTTPS_PROXY` are set to different values
- **THEN** the configured `network.proxy.url` is used

### Requirement: Environment proxy handling
Without an explicit proxy, qmtcode SHALL honor `HTTPS_PROXY`/`https_proxy` for HTTPS destinations and `HTTP_PROXY`/`http_proxy` for HTTP destinations, in that order, consistently across the mesh relay, first-party HTTP clients and hf-hub.

#### Scenario: HTTPS_PROXY used for the relay
- **WHEN** only `HTTPS_PROXY=http://proxy:8080` is set
- **THEN** the mesh relay connection uses `proxy:8080`

### Requirement: Proxy bypass list
`network.proxy.no_proxy` (or `NO_PROXY`/`no_proxy` when not configured) SHALL list hosts, domain suffixes, IPs or CIDRs that bypass the proxy; `*` bypasses all. Bypassed destinations SHALL be connected directly.

#### Scenario: Self-hosted relay bypasses the proxy
- **WHEN** `no_proxy = [".corp.internal"]` and the mesh relay is `https://relay.corp.internal`
- **THEN** the mesh relay is connected directly without the proxy

### Requirement: Proxy can be disabled
`network.proxy.mode = "off"` (or `--no-proxy`) SHALL disable all proxy usage, including environment variables.

#### Scenario: Proxy disabled despite environment
- **WHEN** `HTTPS_PROXY` is set and `mode = "off"`
- **THEN** all outbound connections are made directly

### Requirement: Basic proxy authentication
Proxy URLs MAY contain `user:password` credentials, which SHALL be sent as HTTP Basic `Proxy-Authorization`. Credentials SHALL be redacted in logs, errors, status output and diagnostics.

#### Scenario: Authenticated proxy
- **WHEN** `url = "http://alice:s3cret@proxy:3128"`
- **THEN** the proxy receives Basic credentials for `alice`
- **AND** mesh status shows `http://alice:***@proxy:3128`

### Requirement: Invalid proxy configuration fails at startup
An unparsable proxy URL or an unsupported scheme SHALL fail startup with an error naming the source (config key, flag or environment variable), with credentials redacted.

#### Scenario: Unsupported scheme
- **WHEN** `url = "ftp://proxy:21"`
- **THEN** startup fails with an error naming `network.proxy.url` and the supported schemes

### Requirement: Direct mesh paths are not proxied
The proxy SHALL apply only to HTTP(S) traffic (relay, pkarr/DNS-over-HTTPS, downloads, APIs). Direct QUIC/UDP mesh paths and network probes SHALL NOT use the proxy.

#### Scenario: Direct path after relay via proxy
- **WHEN** the relay connection goes through the proxy and UDP to the peer is allowed
- **THEN** iroh may still establish a direct QUIC path to the peer
