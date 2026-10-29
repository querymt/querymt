# Proposal

## Why

`qmtcode --mesh-join` fails on enterprise networks that intercept TLS. On a FortiGate network the n0 relay certificate was re-signed by `O=Fortinet, CN=FG10E1TB21902209`. The iroh endpoint trusts only the Mozilla roots compiled into the binary, so it rejects that certificate and never connects to a relay. The user then sees a misleading `no node manager for … in scope iroh:…` after 15s.

The rest of the binary is inconsistent:
- LLM provider calls (reqwest 0.13) use the OS verifier.
- hf-hub downloads and OTLP export use built-in roots.
- None of them can be given an extra corporate CA.
- Proxy support for the mesh is not wired up at all.

iroh 1.4.x fixes proxy authentication and pkarr proxying, and makes the hooks we need usable.

## What Changes

- **iroh upgrade.** Upgrade iroh and iroh-relay from 1.3.0 to 1.4.1 in both the `libp2p-iroh` fork (`/Users/wiking/qmt/libp2p-iroh`, branch `iroh_v1`) and querymt. The 1.4.x releases bring:
  - the rustls-webpki fix
  - correct `Proxy-Authorization`
  - pkarr sent through the proxy
  - net_report no longer using environment proxies
  - captive-portal timeouts
  - relay retry fixes
- **New libp2p-iroh options.** Extend libp2p-iroh's `TransportConfig` with an optional CA/TLS trust config and an explicit proxy URL. Existing `relay_mode` (custom relays, relay auth tokens) is passed through from querymt.
- **Shared network trust policy** for the whole binary, under a new `[network.tls]` config section, CLI flags and environment variables:
  - `extra_ca_certs`: PEM files merged on top of the base roots. This is the primary, recommended mechanism.
  - `roots = "default" | "embedded" | "system"`. `default` keeps each component's current base roots. `system` opts into the OS platform verifier.
- **Mesh trust override.** A per-mesh `iroh_tls = "default" | "system"` (plus optional mesh-only `extra_ca_certs`) overrides the shared policy for the iroh endpoint. Embedded roots plus extra CA files remain the mesh default. The platform verifier is opt-in.
- **Shared network proxy policy**, under a new `[network.proxy]` section:
  - explicit `url` and `no_proxy`, with credential redaction
  - otherwise standard `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY` handling, applied consistently
  - applied to the mesh relay connection, reqwest clients and hf-hub
- **Clients that adopt the shared policy:** all first-party reqwest clients, the hf-hub client (via `HFClientBuilder::client`), the OTLP tonic exporters (trust only) and the iroh endpoint.
- **Configurable mesh relays:** relay URLs (self-hosted `iroh-relay`), an optional relay auth token, and an admission timeout. Defaults stay unchanged (n0 relays, 15s).
- **Diagnostics:**
  - Admission timeout errors include relay connection state and a remediation hint (e.g. `UnknownIssuer` → "TLS interception: add the corporate CA via extra_ca_certs or iroh_tls=system").
  - Mesh status reports the effective trust mode and proxy (credentials redacted).
  - A new `qmtcode --mesh-doctor` command checks DNS, relay TLS (showing the certificate issuer), the WebSocket upgrade through the proxy, and relay connectivity.
- **Documentation:** an "Enterprise networks" guide covering:
  - the allowlist for IT (`*.relay.n0.iroh.link:443`, `dns.iroh.link`, UDP for direct paths)
  - asking IT to exempt the relay from TLS inspection
  - CA/proxy configuration
  - self-hosting a relay
  - known limitations

Not breaking: every new setting defaults to today's behavior.

## Capabilities

### New Capabilities
- `network-tls-trust`: Which certificate authorities outbound TLS connections trust: base-root selection, extra CA files, the mesh override, and failure behavior. Applies consistently across all first-party clients.
- `network-proxy`: How outbound HTTP(S) traffic is routed through forward proxies (explicit config, environment, NO_PROXY, credential handling) across the mesh relay, HTTP clients and model downloads.
- `mesh-relay-transport`: Configuration of the iroh mesh relay path: relay selection (default or self-hosted), relay authentication, and admission timeout.
- `mesh-connectivity-diagnostics`: How mesh connectivity problems are reported: admission failure errors, status reporting, and the `--mesh-doctor` command.

### Modified Capabilities
<!-- None: no existing spec covers networking or the mesh. -->

## Impact

- **External repo:** `/Users/wiking/qmt/libp2p-iroh`, branch `iroh_v1`:
  - iroh `1.3` → `1.4.1`
  - `TransportConfig` gains `ca_tls_config` and `proxy_url`
  - crate version bump to `0.2.0` (new public fields)
  - push; querymt's `Cargo.lock` then moves to the new revision
- **Dependencies:**
  - `iroh`/`iroh-relay` `1.4.1` in `crates/querymt-remote`
  - `iroh/platform-verifier` feature enabled for the `system` mode
  - `rustls-pki-types`, already transitive, used for PEM parsing
  - `rustls-native-certs` is not needed
- **Code:**
  - `crates/querymt-remote` (`mesh_runtime.rs` `iroh_transport_config`, `mesh_runtime_config.rs`, `iroh_diagnostics.rs`, new `iroh_config` trust/proxy/relay types)
  - `crates/agent` (`config/mesh.rs`, `config/single.rs`, `config/quorum.rs`, a new `config/network.rs`, `agent/remote/admission.rs`, `control/mesh.rs`, mesh join/startup wiring, reqwest builders in `tools/builtins/{web_fetch,browse}.rs` and `agent/protocol.rs`)
  - `crates/querymt-utils` (new shared `net` module, `telemetry.rs`, `providers.rs`, `oauth.rs`)
  - `crates/provider-common` (hf-hub clients)
  - `crates/service-client`
  - `crates/querymt` (extism host HTTP client)
  - `crates/agent/examples/qmtcode.rs` (CLI flags, `--mesh-doctor`)
- **Docs:** `docs/docs/agent/mesh.md` and a new enterprise networking page.
- **Known gaps (documented, not fixed):**
  - Third-party transitive clients ignore the policy: mistral.rs → hf-hub 0.4.3 / ureq 2 with webpki-roots 0.26, and izwi → reqwest 0.11 with webpki-roots 0.25.
  - OTLP gRPC export cannot go through an HTTP proxy.
  - PAC/WPAD and NTLM/Kerberos proxy authentication are out of scope.
