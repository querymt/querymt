# Spec Delta

## Purpose

Controls which certificate authorities outbound TLS connections trust, so qmtcode works on networks that intercept TLS (e.g. FortiGate deep inspection) while keeping today's behavior by default.

## ADDED Requirements

### Requirement: Default trust is unchanged
With no trust configuration, every outbound TLS client SHALL keep its current base roots: embedded Mozilla roots for the iroh mesh endpoint and OTLP export, and the OS verifier for first-party HTTP clients.

#### Scenario: No trust configuration
- **WHEN** qmtcode starts without `[network.tls]`, `iroh_tls`, trust flags or trust environment variables
- **THEN** the mesh endpoint verifies relay certificates against the embedded roots only
- **AND** a certificate signed by an unknown corporate CA is rejected

### Requirement: Extra CA certificate files
Users SHALL be able to add PEM CA files through `network.tls.extra_ca_certs`, the repeatable `--extra-ca-cert <path>` flag, or the `QMT_EXTRA_CA_CERTS` environment variable (path-list separated). These CAs SHALL be merged with the base roots of every first-party TLS client, never replacing them.

#### Scenario: Corporate CA added
- **WHEN** `extra_ca_certs` points at a PEM file containing the FortiGate CA
- **AND** the network re-signs the relay certificate with that CA
- **THEN** the mesh relay TLS handshake succeeds
- **AND** hf-hub downloads, OTLP export and provider HTTP calls also accept certificates issued by that CA

#### Scenario: Public certificates still trusted
- **WHEN** extra CA files are configured
- **THEN** servers presenting certificates from public CAs in the base roots are still accepted

### Requirement: Sources of extra CAs are combined
Extra CA certificates from config, CLI and environment SHALL be combined and de-duplicated, not override one another. A file MAY contain several certificates.

#### Scenario: Config and environment both set
- **WHEN** the config lists `a.pem` and `QMT_EXTRA_CA_CERTS` lists `b.pem`
- **THEN** CAs from both files are trusted

### Requirement: Base root selection
`network.tls.roots` (and `--tls-roots`) SHALL accept `default`, `embedded` or `system`. `embedded` uses compiled-in Mozilla roots everywhere. `system` uses the operating system verifier everywhere it is supported. `default` keeps each client's current base roots.

#### Scenario: System roots selected
- **WHEN** `roots = "system"` and the corporate CA is installed in the OS trust store
- **THEN** the mesh relay, hf-hub, OTLP and HTTP clients accept certificates issued by that CA without `extra_ca_certs`

### Requirement: Mesh trust override
The mesh config SHALL accept `iroh_tls = "default" | "system"` and a mesh-only `extra_ca_certs`. When set, these SHALL override the shared trust policy for the iroh endpoint only. The mesh default SHALL be embedded roots plus any shared and mesh extra CA files. The platform verifier is used only when `iroh_tls = "system"` or shared `roots = "system"` applies.

#### Scenario: Mesh opts into the system verifier
- **WHEN** `[mesh] iroh_tls = "system"` and the shared policy is left at default
- **THEN** the iroh endpoint uses the OS verifier
- **AND** other clients keep their default roots

#### Scenario: Mesh-only CA
- **WHEN** `[mesh] extra_ca_certs = ["corp.pem"]` is set and the shared policy has no extras
- **THEN** only the iroh endpoint trusts `corp.pem`

### Requirement: Invalid trust configuration fails at startup
An unreadable path, a file with no valid PEM certificate, or an unknown roots mode SHALL fail startup with an error naming the source (config key, flag or environment variable) and the path. Trust SHALL never be silently weakened or ignored.

#### Scenario: Missing CA file
- **WHEN** `extra_ca_certs = ["/missing.pem"]`
- **THEN** qmtcode exits with an error naming `network.tls.extra_ca_certs` and `/missing.pem`

#### Scenario: No certificates in file
- **WHEN** an extra CA file contains no PEM `CERTIFICATE` blocks
- **THEN** startup fails with an error saying the file contains no certificates

### Requirement: Certificate verification is never disabled
No configuration option SHALL disable server certificate verification for outbound TLS.

#### Scenario: Interception without trusted CA
- **WHEN** the network re-signs certificates with a CA that is not trusted by any configured source
- **THEN** the connection fails with a certificate verification error rather than connecting
