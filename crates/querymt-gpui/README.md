# QueryMT GPUI (prototype)

GPUI rebuild of QueryMT Desktop. One Rust UI, two targets: a native window and a web app
(wasm + WebGPU/WebGL2). Scope so far: the QueryMT shell, the **Start** page and the **Agents** page,
wired to real agents over ACP.

## Native

```bash
nix develop -c cargo run                                   # qmtcode on PATH is added as "Local qmtcode"
nix develop -c cargo run -- --agent 127.0.0.1:42069        # add a WebSocket agent (also QMT_AGENT=...)
QMT_DEBUG=1 nix develop -c cargo run                       # log agent/RPC traffic to stderr
```

Agents are stored in `~/.config/querymt-gpui/state.json`.

### Release

```bash
nix develop -c cargo build --release       # target/release/querymt-gpui
```

The desktop has only a binary target: no application `.so`, `.dylib`, `.dll`, or
`.rlib` is built. Rust dependency archives remain internal Cargo build artifacts.
Release builds use `opt-level = "z"`, full LTO, one codegen unit, no debug symbols,
and abort-on-panic (a panic terminates the app instead of unwinding).
System graphics/windowing libraries are still required; this is not a fully static
portable Linux binary. Size optimization can reduce peak throughput; use debug
builds when you need symbols and panic backtraces.

## Web

```bash
nix develop -c ./web/build.sh --release
nix develop -c miniserve --index index.html dist           # http://localhost:8080
```

- Served by qmtcode itself (embedded): the page connects to its own origin `/acp/ws` automatically.
- `?agent=host:port` points the page at another server. `?page=agents&details=1` deep-links.
- qmtcode rejects WebSocket upgrades from a different Origin (HTTP 403). Cross-origin testing needs
  the page served from the same origin as qmtcode, or a proxy such as
  `websocat -B 4000000 ws-l:127.0.0.1:8100 ws://127.0.0.1:42069/acp/ws`.
- Agents are stored in `localStorage`.

## Layout

- `src/transport.rs`: WebSocket (native thread + rustls, browser `WebSocket`) and stdio process transport
- `src/rpc.rs`: JSON-RPC 2.0 client
- `src/store.rs`: agents store (configs, persistence, connect/disconnect, capabilities, models, sessions, mesh, launch)
- `src/theme.rs`: QueryMT design tokens (light/dark) applied to gpui-component
- `src/ui/`: shell + sidebar (`mod.rs`), `start.rs`, `agents.rs`, shared bits (`kit.rs`)
- `src/main.rs`: shared UI and native/WASM entry points (no library target)
- `build.rs`: downloads and caches the pinned IBM Plex fonts for embedding
- `assets/`: the QueryMT mark and provider icons

## Dependencies

GPUI Kit is consumed from crates.io without a local fork. Its theme controls the
rounded Linux window frame, shadows, and resize handles; the app owns only its header.
`src/highlight.rs` adapts the standalone syntect highlighting path from gpui-component
0.9.0 for tool output and Markdown fences. The direct `syntect` and `two-face`
dependencies share the component library's backend and features rather than adding
another grammar stack.

The first build downloads five IBM Plex fonts from a pinned upstream commit and
verifies their SHA-256 checksums. Downloads are cached under the OS cache directory
(`~/.cache/querymt-gpui/fonts` on Linux), shared across native/WASM builds and kept
after `cargo clean`. Set `QMT_FONT_CACHE` to use a different cache root, for example
for CI caching or a pre-populated offline build. Subsequent builds use cached files
without network requests. `CARGO_NET_OFFLINE=true` rejects missing or corrupt fonts
rather than downloading them; the first uncached build needs network access.

## Web caveats

- Icons for the web are embedded with `icon_assets!` in `src/main.rs`. Add new icon names there.
- `RUSTC_BOOTSTRAP=1` (set in `build.sh`) is needed for `wasm_thread`'s nightly feature gate.
- The `wasm-bindgen` CLI in `flake.nix` must match `wasm-bindgen` in `Cargo.lock` (pinned `=0.2.130`).
