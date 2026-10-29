# Tasks

## 1. libp2p-iroh: upgrade to iroh 1.4.1 (`/Users/wiking/qmt/libp2p-iroh`, branch `iroh_v1`)

- [ ] 1.1 Tag the current commit `e8fa6c2` as `v0.1.3-iroh1.3` for rollback and verify with `git tag --list`
- [ ] 1.2 Bump `iroh = "1.4.1"` in `Cargo.toml`, run `cargo update -p iroh -p iroh-relay`, fix compile errors, and verify `cargo test --all-features` and `cargo clippy --all-targets` pass
- [ ] 1.3 Confirm `CaTlsConfig`, `RelayConfig::with_auth_token`, `Builder::proxy_url` and `Builder::ca_tls_config` exist with unchanged signatures in iroh 1.4.1 and verify by compiling a scratch use in `src/transport.rs` tests
- [ ] 1.4 Commit "chore: update to iroh 1.4.1" separately from feature work and verify `git log` shows it as its own commit

## 2. libp2p-iroh: TransportConfig trust and proxy fields

- [ ] 2.1 Add `ca_tls_config: Option<iroh::tls::CaTlsConfig>` and `proxy_url: Option<url::Url>` to `TransportConfig` (default `None`), apply them on the endpoint builder in `Transport::with_config`, and verify with a unit test that a default config builds unchanged
- [ ] 2.2 Redact proxy credentials in `TransportConfig`'s `Debug` and verify with a test asserting the password does not appear in `format!("{:?}")`
- [ ] 2.3 Add feature `platform-verifier = ["iroh/platform-verifier"]` and verify `cargo check --features platform-verifier` succeeds
- [ ] 2.4 Add an integration test using a local `iroh_relay::server` with a self-signed CA: the endpoint fails to reach the relay with no extra roots and succeeds with `CaTlsConfig::embedded().with_extra_roots(..)`; verify with `cargo test`
- [ ] 2.5 Add an integration test with a minimal local HTTP CONNECT proxy, verifying the relay connection goes through `proxy_url` (proxy saw a CONNECT for the relay host)
- [ ] 2.6 Document the new fields in `README.md`, bump the crate version to `0.2.0`, push `iroh_v1`, and verify the remote branch head matches the local commit

## 3. querymt: iroh 1.4.1 upgrade (no behavior change)

- [ ] 3.1 Bump `iroh`/`iroh-relay` to `1.4.1` (normal and dev deps) in `crates/querymt-remote/Cargo.toml`, run `cargo update -p libp2p-iroh -p iroh -p iroh-relay`, and verify `Cargo.lock` shows iroh 1.4.1 and the new libp2p-iroh revision
- [ ] 3.2 Fix compile errors (struct literals for `TransportConfig` now use `..Default::default()`), and verify `cargo nextest run -p querymt-remote --all-features` and `cargo nextest run -p querymt-agent --features remote` pass
- [ ] 3.3 Verify that `iroh_gso_is_forwarded_for_iroh_only_and_mixed_transports` and the `iroh_diagnostics` relay tests still pass, then commit the upgrade on its own

## 4. Shared network policy (`querymt-utils::net`)

- [ ] 4.1 Create `querymt_utils::net` with `NetworkPolicyConfig` (serde/schemars, `deny_unknown_fields`), `TlsPolicy`, `RootsMode {Default, Embedded, System}`, `ProxyPolicy`, and verify config round-trips with unit tests
- [ ] 4.2 Implement PEM loading (multiple certs per file, DER de-dup, source tracking) and verify tests for a missing file, a file without certificates, a multi-cert file and duplicates across sources
- [ ] 4.3 Implement `resolve(config, cli, env)` with the precedence in design D5/D6 (including `QMT_EXTRA_CA_CERTS`, `QMT_TLS_ROOTS`, `HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY` and lowercase variants) and verify table-driven tests with an injected env map
- [ ] 4.4 Implement the `no_proxy` matcher (host, suffix, IP, CIDR, `*`) and proxy URL validation (http/https only) and verify unit tests
- [ ] 4.5 Implement redaction helpers for proxy userinfo and tokens and verify tests that secrets never appear in `Display`/`Debug`
- [ ] 4.6 Implement `apply_reqwest` (`tls_certs_merge`, `tls_certs_only` with webpki roots for `embedded`, `Proxy::custom` with the shared matcher, `no_proxy()` for off) and verify with a test against a local TLS server signed by a test CA (fails without extras, succeeds with extras)
- [ ] 4.7 Implement `tonic_tls_config()` (webpki/native roots plus `ca_certificates`), enable the tonic `tls-native-roots` feature, and verify a unit test builds the config for each mode
- [ ] 4.8 Add a process-wide `install()`/`policy()` `OnceLock` plus `resolve_early()` (env/CLI only) and verify that `policy()` returns the default when not installed

## 5. Adopt the shared policy across the binary

- [ ] 5.1 Add a `[network]` section to the single and quorum agent configs, wiring `NetworkPolicyConfig`, and verify config loader tests parse the example TOML
- [ ] 5.2 Add qmtcode flags `--extra-ca-cert` (repeatable), `--tls-roots`, `--proxy` and `--no-proxy`, resolve and `install()` the policy right after config load, and verify CLI parsing tests in `qmtcode.rs`
- [ ] 5.3 Route first-party reqwest builders through `net::policy().apply_reqwest(..)`: `tools/builtins/{web_fetch,browse}.rs`, `agent/protocol.rs`, `querymt-utils/{providers,oauth}.rs`, `service-client/src/lib.rs` (both), extism host `functions.rs`. Verify `cargo check --workspace --all-features` and that a search finds no remaining bare `reqwest::Client::new()` in first-party code
- [ ] 5.4 Build hf-hub clients in `provider-common` with an injected policy-aware `reqwest::Client` (`HFClientBuilder::client` + explicit user agent; `build_sync` for the sync path) and verify existing provider-common tests plus a test that the builder receives the custom client
- [ ] 5.5 Use `tonic_tls_config()` in the three OTLP exporters in `telemetry.rs` (via `resolve_early()` when initialized before config) and verify telemetry tests
- [ ] 5.6 Verify an integration test: with `QMT_EXTRA_CA_CERTS` pointing at a test CA, a reqwest client from the policy and an hf-hub client both reach a local HTTPS server signed by that CA

## 6. Mesh trust, proxy and relay configuration

- [ ] 6.1 Add `IrohTlsConfig` (mode + extra certs), `IrohProxyConfig` and `IrohRelayConfig` (urls, token) to `querymt-remote::iroh_config` with conversions to `CaTlsConfig`/`Url`/`RelayMode` behind `kameo-mesh`, and verify unit tests for each conversion, including `relays = []` → `RelayMode::Disabled`
- [ ] 6.2 Enable the `libp2p-iroh/platform-verifier` feature through querymt-remote's `kameo-mesh` (desktop) and reject `system` with a clear error when it is unavailable; verify with `cargo check` with and without the feature
- [ ] 6.3 Extend `MeshRuntimeConfig` with `iroh_tls`, `iroh_proxy`, `iroh_relays` and `admission_timeout` and pass them in `iroh_transport_config`, extending the existing `iroh_gso_is_forwarded…` test to assert they are forwarded
- [ ] 6.4 Add `[mesh] iroh_tls`, `extra_ca_certs`, `relays`, `relay_auth_token` (+ `QMT_MESH_RELAY_TOKEN`) and `admission_timeout` (1s–10min) to `MeshTomlConfig`, merge with the shared policy per design D4, and verify config tests, including validation errors that name the key
- [ ] 6.5 Add qmtcode flags `--mesh-iroh-tls` and `--mesh-relay` (repeatable), wire them through the mesh startup and join paths (`agent/remote/mesh.rs`, `mesh/join.rs`, `api/mesh/runtime_config.rs`), and verify qmtcode argument tests
- [ ] 6.6 Replace `AdmissionPolicy::production()` with a configurable `max_elapsed` and verify the existing admission tests plus a new test for a custom timeout
- [ ] 6.7 Verify an end-to-end test in `querymt-remote`: a local relay with a test CA plus an invite join succeeds only when the joiner has the CA in `extra_ca_certs`; a variant with a relay requiring a bearer token succeeds only with a matching `relay_auth_token`

## 7. Diagnostics

- [ ] 7.1 Add a pure `classify_network_error` hint function (TLS / proxy / relay token / DNS) and verify unit tests with real error strings, including `UnknownIssuer`
- [ ] 7.2 Extend `AdmissionError::NoReachablePeer` with relay snapshots, `inviter_connected` and `hint`, populated from `MeshHandle` diagnostics, and verify an admission test with a fake transport and a failing relay asserts the message contents
- [ ] 7.3 Add a `network` object (trust mode, extra CA count, redacted proxy and source, relays, `relay_token_set`) to mesh status in `control/mesh.rs`, and verify the control tests and the typeshare/TS types regenerate
- [ ] 7.4 Implement the `--mesh-doctor [--json]` checks (DNS, TLS issuer via `x509-parser`, WebSocket upgrade through the proxy, iroh `online()` within 10s) with exit codes, and verify tests against a local relay (healthy → 0) and against a relay with an untrusted CA (non-zero, issuer printed, hint shown)
- [ ] 7.5 Verify that doctor JSON output is a single document with redacted secrets, via a snapshot test

## 8. Documentation

- [ ] 8.1 Write `docs/docs/agent/enterprise-networks.md`: an IT allowlist (`*.relay.n0.iroh.link:443`, `dns.iroh.link`, UDP for direct paths), a TLS inspection exemption request, `extra_ca_certs`/`iroh_tls`/`roots`, proxy config, self-hosted relay with token, `--mesh-doctor`, and known gaps (mistral.rs/izwi transitive clients, OTLP proxy, PAC/NTLM). Verify the docs build and that every config key in the page parses in a config test
- [ ] 8.2 Update `docs/docs/agent/mesh.md` and the example configs with `[network]` and the new `[mesh]` keys, and verify the example configs load in existing example-config tests

## 9. Integration verification

- [ ] 9.1 Run `cargo nextest run --workspace --all-features` and `cargo clippy --workspace --all-targets --all-features -- -D warnings` and verify both pass
- [ ] 9.2 Manually reproduce on the FortiGate network: `qmtcode --mesh-doctor` shows the `O=Fortinet` issuer and fails; with `--extra-ca-cert fortinet.pem` (or `--mesh-iroh-tls system` with the CA installed) the doctor passes and `--mesh-join` succeeds. Record the outcome in the change notes

## Workflow follow-up

- Archive the change after review and after the FortiGate verification (9.2) is recorded.
