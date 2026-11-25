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

### Run or install with Nix

```bash
nix run github:querymt/querymt-gpui                          # build and run without installing
nix run github:querymt/querymt-gpui -- --agent 127.0.0.1:42069
nix profile install github:querymt/querymt-gpui              # install (adds a "QueryMT" launcher entry)
nix build                                                    # local checkout: ./result/bin/querymt-gpui
```

The package embeds the pinned IBM Plex fonts (pre-fetched, since the Nix build has no network),
installs a desktop entry and icon, and wraps the binary with the Vulkan/Wayland/X11 libraries GPUI
loads at runtime.

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

### Quick panel

A small floating chat you open from any application, for a quick question, nudging a running
session or answering its permission prompt, without changing what the main window shows.
Desktop only: the web build has no quick panel.

Turn it on in **Settings → General → Quick panel**. It is off by default. While it is on:

- A global shortcut toggles the panel: **Option+Space** on macOS, **Ctrl+Alt+Space** on Windows
  and Linux. Change it in the same row (click the shortcut, press the new keys). If the shortcut
  is taken by another application, Settings says so and the previous shortcut stays active.
- **Esc**, the shortcut again, or clicking another window closes the panel. Reopening it within
  15 minutes returns to the same conversation and draft; after that it starts on a new quick chat.
- **Enter** sends, **Shift+Enter** breaks the line, **Cmd+Enter** (**Ctrl+Enter** elsewhere) opens
  the session in the main window.
- Closing the main window does not quit the app. A tray icon offers *Open QueryMT*, *Quick panel*
  and *Quit*.

`--quick` toggles the panel of the running instance and exits (it does not start a second app).
When nothing is running it starts the app with only the panel open:

```bash
cargo run -- --quick              # from a checkout
querymt-gpui --quick              # installed binary
```

Without the quick panel enabled, `--quick` is an ordinary start.

**Wayland.** There is no global shortcut API on every compositor. QueryMT asks the desktop's
GlobalShortcuts portal for the shortcut (KDE Plasma, GNOME 48 and later; the desktop may ask you to
approve it). Where there is no such portal, Settings says the shortcut is unavailable. Bind the
command `querymt-gpui --quick` to a key in your desktop's own keyboard shortcut settings instead
(on GNOME: *Settings → Keyboard → Custom Shortcuts*).

**Linux tray.** The tray icon uses the StatusNotifierItem D-Bus protocol: no GTK or
libappindicator is needed, but the desktop must show such icons (KDE and most panels do; GNOME
needs the AppIndicator/KStatusNotifierItem extension). Without a tray host the shortcut and
`--quick` still work.

**Window placement.** On macOS and Windows the panel is a floating window above other
applications, centered on the main window's display (or the primary display). X11 uses a floating
window and Wayland compositors with layer-shell a layer surface; GNOME opens a normal window,
which may appear behind a fullscreen application.

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
