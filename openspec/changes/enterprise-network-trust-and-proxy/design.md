# Design

## Context

See proposal.md (Why). Current state relevant to the approach:

- **libp2p-iroh fork** (`/Users/wiking/qmt/libp2p-iroh`, branch `iroh_v1`, HEAD `e8fa6c2`, crate `0.1.3`) builds the endpoint inside `Transport::with_config`. It uses `iroh::Endpoint::builder(presets::N0)` with `relay_mode`, `transport_config` and an optional `dns_resolver`. It never calls `ca_tls_config`, `proxy_url` or `proxy_from_env`. It depends on `iroh = "1.3"` with `default-features = false, features = ["tls-ring"]`.
- **querymt-remote** maps `MeshRuntimeConfig` to `libp2p_iroh::TransportConfig` in `mesh_runtime.rs::iroh_transport_config`. Only timeout, GSO and DNS are passed. `relay_mode` is left at `Default`. `IrohDnsConfig` in `iroh_config.rs` is the existing pattern for a serde/schemars policy type that converts to an iroh type behind the `kameo-mesh` feature.
- **Agent config:** `MeshTomlConfig` (`crates/agent/src/config/mesh.rs`) is embedded in `SingleAgentConfig` and the quorum config. `qmtcode.rs` already has CLI overrides for mesh settings (`--mesh-iroh-gso`, `mesh_dns`).
- **Admission:** `admission.rs` uses a fixed `AdmissionPolicy::production()` of 15s. Its errors carry only the last lookup error. `IrohDiagnostics` already exposes per-relay `connected`, `last_error` and `auth_denied_reason`.
- **HTTP clients:**
  - reqwest 0.13 (`rustls` feature) defaults to `rustls-platform-verifier`. It supports `tls_certs_merge`, `tls_certs_only` and `proxy`/`no_proxy`.
  - First-party builders: `tools/builtins/{web_fetch,browse}.rs`, `agent/protocol.rs`, `querymt-utils/{providers,oauth}.rs`, `service-client/src/lib.rs` (two), `querymt/src/plugin/extism_impl/host/functions.rs`.
  - hf-hub 1.0 (`provider-common`) builds its own reqwest 0.13 client, but `HFClientBuilder::client(reqwest::Client)` accepts one from us.
  - OTLP (`querymt-utils/telemetry.rs`) uses tonic `ClientTlsConfig::new().with_enabled_roots()` with `tls-webpki-roots`. tonic supports `ca_certificates(..)` and `with_native_roots()` (feature `tls-native-roots`).
- **Transitive clients we don't control:** mistral.rs uses hf-hub 0.4.3 (ureq 2 + webpki-roots 0.26) and reqwest 0.12. izwi uses reqwest 0.11 + webpki-roots 0.25.

## Goals / Non-Goals

**Goals:**
- One resolved network policy (trust + proxy), computed once at startup and handed to every first-party client builder.
- Mesh default: embedded roots plus extra CA files. The platform verifier is opt-in (`iroh_tls = "system"`).
- Upgrade iroh to 1.4.1 in both repos, with no behavior change at default settings.
- Make failures self-explanatory (admission error, status, `--mesh-doctor`).

**Non-Goals:**
- PAC/WPAD, NTLM/Kerberos/Negotiate proxy authentication, SOCKS proxies.
- Patching third-party transitive HTTP stacks (mistral.rs, izwi). These are documented as gaps.
- Proxying OTLP gRPC export.
- Running or bundling a relay server. Self-hosting is documented only.
- Disabling certificate verification in any form.

## Decisions

### D1. Upgrade sequencing: fork first, then querymt
1. In libp2p-iroh, bump `iroh = "1.4.1"`, run its tests, then add the new `TransportConfig` fields (D3).
2. Bump the crate to `0.2.0`, because adding public struct fields is breaking for struct-literal users. Push to `iroh_v1`.
3. In querymt, bump `iroh`/`iroh-relay` to `1.4.1` in `crates/querymt-remote/Cargo.toml` (both normal and dev deps). Run `cargo update -p libp2p-iroh -p iroh -p iroh-relay`, then fix compile errors.
4. The upgrade alone is its own commit/PR step, so regressions can be bisected away from feature work.

*Alternative:* a path dependency during development. Allowed locally via `[patch."https://github.com/querymt/libp2p-iroh.git"]`, but never committed.

### D2. Shared policy lives in `querymt-utils::net`
A new `querymt_utils::net` module holds these types: `NetworkPolicy { tls: TlsPolicy, proxy: ProxyPolicy }`, `TlsPolicy { roots: RootsMode, extra_ca_certs: Vec<CertificateDer<'static>>, sources }` and `ProxyPolicy { mode, url, no_proxy, source }`. It provides:
- `NetworkPolicyConfig` (serde, schemars, `deny_unknown_fields`), deserialized from `[network]`.
- `resolve(config, cli, env) -> Result<NetworkPolicy>`: loads PEM files and merges sources (D5).
- Adapters: `apply_reqwest(ClientBuilder) -> ClientBuilder` and `tonic_tls_config() -> ClientTlsConfig`.
- `redacted()` display helpers.

`querymt-remote` gets its own small trust/proxy types (`IrohTlsConfig`, `IrohProxyConfig`) that mirror the `IrohDnsConfig` pattern and convert to iroh types. This avoids making `querymt-remote` depend on `querymt-utils`. The agent converts the resolved `NetworkPolicy` plus mesh overrides into these types.

*Alternative:* a new `querymt-net` crate. Rejected for now: `querymt-utils` is already a dependency of every consumer that needs the policy.

### D3. libp2p-iroh `TransportConfig` additions
- `ca_tls_config: Option<iroh::tls::CaTlsConfig>`. When `Some`, call `.ca_tls_config(..)` on the builder; when `None`, keep the upstream default (embedded).
- `proxy_url: Option<url::Url>`. When `Some`, call `.proxy_url(..)`. We never call `proxy_from_env`, because querymt resolves the environment itself (D6) for consistent `NO_PROXY` handling.
- `relay_mode` already exists. querymt now sets it to `RelayMode::Custom(RelayMap)` with per-relay `RelayConfig::with_auth_token` when a token is configured, or `RelayMode::Disabled` for `relays = []`.
- Implement `Debug` for the new fields with redaction of the proxy credentials.
- Add a feature `platform-verifier = ["iroh/platform-verifier"]` in the fork, re-exported through querymt-remote's `kameo-mesh` feature, so `CaTlsConfig::system()` is available.

### D4. Trust resolution
Base roots by mode:

| mode | iroh endpoint | reqwest 0.13 | tonic (OTLP) |
|---|---|---|---|
| `default` | embedded (`CaTlsConfig::embedded()`) | platform verifier (reqwest default) | webpki roots (current) |
| `embedded` | embedded | `tls_certs_only(webpki roots + extras)` | `with_webpki_roots()` |
| `system` | `CaTlsConfig::system()` | platform verifier | `with_native_roots()` |

Extras are always merged on top: `CaTlsConfig::with_extra_roots`, reqwest `tls_certs_merge`, tonic `ca_certificates`.

Mesh override: `[mesh] iroh_tls = "default" | "system"`. `default` defers to the shared `roots`, with the shared `default` meaning embedded for the mesh. `system` forces the platform verifier. Mesh `extra_ca_certs` are appended to the shared extras for the iroh endpoint only.

*Why extras by default and system as opt-in:* extra CAs behave the same on every OS, need no extra dependencies at runtime, and keep the corporate CA scoped to qmtcode. The platform verifier is right for managed fleets but adds platform-specific behavior, including Android/iOS mobile FFI considerations.

Implementation note for reqwest `embedded`: this needs `webpki-root-certs` (already transitive via iroh-relay) to obtain a `Certificate` list. If reqwest's `tls_certs_merge` with the platform verifier fails for some backend, `build()` returns an error. We surface it at startup and do not ignore it.

### D5. Source precedence and merging
- `roots`: CLI `--tls-roots` > config `network.tls.roots` > env `QMT_TLS_ROOTS` > `default`.
- `extra_ca_certs`: the union of CLI `--extra-ca-cert` (repeatable), config, and `QMT_EXTRA_CA_CERTS` (OS path-list separator). Duplicates are removed by DER bytes.
- Each loaded file records its source for error and status messages.
- PEM parsing uses `rustls-pki-types` `CertificateDer::pem_file_iter`. A file must contain at least one certificate.
- We deliberately do not honor `SSL_CERT_FILE` as an extra source. Its meaning ("replace roots") differs between tools. Users can point `QMT_EXTRA_CA_CERTS` at the same file.

### D6. Proxy resolution
- Precedence: CLI `--no-proxy` / `--proxy` > config `network.proxy` (`mode = "auto" | "off"`, `url`, `no_proxy`) > environment (`HTTPS_PROXY`, `https_proxy`, then `HTTP_PROXY`, `http_proxy`; `NO_PROXY`/`no_proxy`).
- Supported schemes: `http`, `https`.
- `no_proxy` matching: exact host, `.suffix` / leading-dot suffix, IP, CIDR, `*`. One matcher is implemented in `querymt_utils::net` and reused by every adapter.
- For reqwest we build `reqwest::Proxy::custom` with that matcher, so behavior matches the iroh side. With `mode = "off"` we call `no_proxy()` on the builder.
- For iroh, the proxy URL is passed only if no relay host matches `no_proxy`. iroh takes a single proxy for all HTTP traffic. If relays are mixed (some bypassed, some not), the proxy is applied and a warning is logged. This is documented as a limitation.
- Credentials: the URL userinfo is kept for the clients (iroh 1.4 sends correct Basic auth; reqwest does too). Every `Display`/`Debug`/status path uses the redacted form.

### D7. hf-hub and OTLP adoption
- `provider-common` builds a `reqwest::Client` via `NetworkPolicy::apply_reqwest(reqwest::Client::builder().user_agent(..))` and passes it with `HFClientBuilder::client(..)`. Because hf-hub ignores `user_agent`/`headers` when a client is supplied, we set the UA ourselves. The sync client uses the same builder via `HFClientBuilder::build_sync()` (hf-hub 1.0 also offers `HFClientSync::from_inner(HFClient)`).
- `telemetry.rs` replaces `ClientTlsConfig::new().with_enabled_roots()` with `policy.tonic_tls_config()`. Add the tonic feature `tls-native-roots` for `system`.
- The policy is installed as a process-wide `OnceLock<NetworkPolicy>` in `querymt_utils::net`, set once by binaries after config load. Library code calls `net::policy()`, which returns the default policy if unset. This avoids threading the policy through every constructor, including extism host functions and OAuth flows. Telemetry initializes before config in some paths, so its init reads the env/CLI portion via `resolve_early()`.

*Alternative:* pass `&NetworkPolicy` explicitly everywhere. Cleaner, but touches many public signatures across crates. Explicit parameters are kept for the mesh path only (where config already flows).

### D8. Relay configuration and admission timeout
- `[mesh] relays: Option<Vec<Url>>`, `relay_auth_token: Option<String>` (env `QMT_MESH_RELAY_TOKEN`), and `admission_timeout: Option<humantime Duration>`, validated to 1s–10min.
- `MeshRuntimeConfig` gains `iroh_relays`, `iroh_tls`, `iroh_proxy` and `admission_timeout`.
- `AdmissionPolicy::production()` becomes `AdmissionPolicy::with_max_elapsed(timeout)`.

### D9. Diagnostics
- `AdmissionError::NoReachablePeer` gains `relays: Vec<IrohRelayDiagnostics>`, `inviter_connected: bool` and `hint: Option<String>`. The join path attaches these from `MeshHandle`'s diagnostics snapshot when the error is built.
- `hint` is computed by a pure classifier over error strings and auth-denied reasons, so it can be unit tested. Patterns: `UnknownIssuer`/`invalid peer certificate` → TLS; `407`/`proxy` → proxy; `auth_denied_reason` → token; DNS resolver errors → `iroh_dns`.
- Mesh status JSON (`control/mesh.rs`) gains a `network` object: `{ trust: { mode, extra_ca_count }, proxy: { url_redacted, source }, relays, relay_token_set }`.
- `--mesh-doctor [--json]` is a new qmtcode mode. Checks:
  1. DNS lookup of each relay host and `dns.iroh.link`, using the configured `iroh_dns` resolver.
  2. TLS probe per relay: rustls over TCP (or CONNECT through the proxy) with the effective trust. Records the leaf issuer DN, parsed with `x509-parser` (already in `Cargo.lock` transitively; added as a direct dependency of the qmtcode/agent doctor module).
  3. WebSocket upgrade `GET /relay` with `Upgrade: websocket`; expects `101`, or any relay-originated non-proxy response.
  4. Bind a temporary iroh endpoint with the effective config and wait for `online()` with a 10s timeout.

  Exit code 0 only if all non-skipped checks pass.

## Risks / Trade-offs

- [iroh 1.4.1 API drift in `CaTlsConfig`, `RelayConfig::with_auth_token`, `proxy_url`] → Verify against 1.4.1 sources in task 1. All were present in 1.3.0, and the 1.4 changelog lists no breaking change.
- [Proxy re-signs WebSocket traffic, or blocks the upgrade, even with a trusted CA] → Doctor check 3 detects it. The docs recommend an IT exemption. iroh-relay falls back to a signed-challenge handshake when TLS keying material differs, which keeps auth working through an intercepting proxy.
- [Global `OnceLock` policy hides dependencies and makes tests order-sensitive] → Tests use explicit `NetworkPolicy` adapters; only binaries call `install()`; `policy()` falls back to the default.
- [`platform-verifier` feature increases dependencies on mobile FFI builds] → It is a cargo feature, enabled for desktop `qmtcode`/agent builds. On mobile, `iroh_tls = "system"` is rejected with a clear error if the feature is absent.
- [Transitive clients (mistral.rs hf-hub 0.4/ureq, izwi reqwest 0.11) ignore the policy] → Documented as a known gap with the workaround (pre-download models, or set the provider's own env such as `HF_ENDPOINT`). Tracked for follow-up upstream.
- [A single proxy for iroh vs. mixed `no_proxy` relays] → Warning plus documentation (D6).
- [Secrets leaking via Debug/logs] → Redaction helpers with unit tests asserting that secrets never appear in `Debug`, `Display`, status JSON or doctor JSON.

## Migration Plan

1. libp2p-iroh: release `0.2.0` on `iroh_v1` (upgrade + fields).
2. querymt: an upgrade-only commit (iroh 1.4.1 + new libp2p-iroh rev, default behavior) with the full test suite green.
3. Feature commits per task group. All settings are opt-in, so no config migration is needed.
4. Rollback: revert querymt to the previous `Cargo.lock` revision of libp2p-iroh. The fork keeps the `0.1.3` commit tagged (`v0.1.3-iroh1.3`).

