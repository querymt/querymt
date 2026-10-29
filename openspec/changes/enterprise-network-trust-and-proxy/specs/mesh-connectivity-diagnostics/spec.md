# Spec Delta

## Purpose

Makes mesh connectivity failures explainable: admission errors, mesh status and a doctor command report the real cause (TLS interception, proxy, DNS, blocked UDP), not just a generic lookup failure.

## ADDED Requirements

### Requirement: Admission failures include relay state
When an invite join times out, the error SHALL include the connection state and last error of each configured relay, and whether any connection to the inviter was established. This is in addition to the existing mesh id, attempts and last lookup error.

#### Scenario: Relay blocked by TLS interception
- **WHEN** a join times out and the relay failed with an unknown-issuer certificate error
- **THEN** the error includes the relay URL and the certificate error
- **AND** states that no connection to the inviter was established

### Requirement: Remediation hints for known failures
Admission errors and doctor output SHALL include a one-line hint for recognized causes:
- certificate errors → TLS interception and how to configure `extra_ca_certs` or `iroh_tls = "system"`
- relay auth denied → relay token
- proxy connect failures → proxy settings
- DNS failures → `iroh_dns`

#### Scenario: Hint for unknown issuer
- **WHEN** the relay error is a certificate verification failure
- **THEN** the message suggests adding the corporate CA via `extra_ca_certs` or setting `iroh_tls = "system"`

### Requirement: Status reports effective network policy
Mesh status SHALL report:
- the effective trust mode for the iroh endpoint (`embedded`, `system`) and the number of extra CAs
- the effective proxy, with credentials redacted, and its source (config, env, off)
- the configured relays and whether a relay token is set

The token value itself SHALL NOT be shown.

#### Scenario: Status with proxy and CA
- **WHEN** a proxy with credentials and one extra CA are configured
- **THEN** status shows trust `embedded + 1 extra CA`, the proxy with redacted password, and source `config`

### Requirement: Mesh doctor command
`qmtcode --mesh-doctor` SHALL run connectivity checks with the effective network policy and exit without starting the agent. Checks:
- DNS resolution of each relay host and the discovery host
- TLS handshake to each relay, reporting the leaf issuer and whether it chains to the configured trust
- relay WebSocket upgrade through the effective proxy
- whether an iroh endpoint reaches a relay within a timeout

#### Scenario: Doctor on an intercepting network
- **WHEN** `qmtcode --mesh-doctor` runs where the relay certificate is issued by `O=Fortinet` and that CA is not trusted
- **THEN** the TLS check fails and prints the issuer `O=Fortinet` and the extra-CA hint
- **AND** the command exits with a non-zero status

#### Scenario: Doctor on a healthy network
- **WHEN** all checks pass
- **THEN** each check prints OK and the command exits with status 0

### Requirement: Doctor output is machine readable on request
`--mesh-doctor --json` SHALL print the check results as JSON. Each check SHALL have a name, a status (`ok`/`fail`/`skip`), details and an optional hint, with secrets redacted.

#### Scenario: JSON output
- **WHEN** `qmtcode --mesh-doctor --json` runs
- **THEN** stdout contains a single JSON document with one entry per check
