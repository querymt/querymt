# Proposal

## Why

Trying out UI ideas in QueryMT Desktop takes a full edit, compile, restart and navigate cycle for every tweak, and each restart loses the open session, scroll position and connection. That makes experiments slow.

GPUI already supports hot-patching. gpui-pre 0.3.14 (pinned by gpui-kit 0.11) has a `hot-reload` feature:

- it routes every view and component render through a `subsecond` hot function;
- it connects to the Dioxus CLI dev server (`dx serve`);
- it applies each patch between frames and refreshes all windows.

`querymt-gpui` is a single binary crate with no library target, which is exactly the shape subsecond can patch: only the crate holding `main.rs` is patched, so all of the app's UI code qualifies. This change wires that up behind an opt-in feature and measures whether it makes the loop fast enough to use.

## What Changes

- **New cargo feature `hot-patch` on `querymt-gpui`** (off by default, native targets only). It enables gpui-pre's `hot-reload` through an optional direct dependency on `gpui-pre` pinned to the same `=0.3.14` gpui-kit uses. gpui-kit does not forward the feature.
- **Debug builds only:** the patch path is compiled only when the feature is on and `debug_assertions` are set (gpui-pre's `build.rs`). Release profiles (`release-gpui`, `release-gpui-web`) and wasm builds are unchanged even if the feature is enabled.
- **Run script** `crates/querymt-gpui/scripts/hot-patch.sh`:
  - checks for the Dioxus CLI 0.7.10, which matches `subsecond` 0.7.10;
  - calls it by its full path, because `dx` on `PATH` may be another tool (Deno's `dx` is installed on the main dev machine);
  - runs `dx serve` with hot-patching, `-p querymt-gpui` and `--features hot-patch`, passing app arguments (`--agent`, `--page`) through.
- **Measurements:** recorded in `tasks.md`:
  - first build time;
  - patch latency for three representative edits;
  - which edits patch, and which need a restart (struct layout changes, key bindings, startup code, thread-locals);
  - whether the quick panel and the native chrome survive a patch.
  - A go or no-go for keeping the feature.
- **Docs:** a "Hot-patching the UI" section in `crates/querymt-gpui/knowledge/development.md`: install, run, what patches, what needs a restart, and troubleshooting.

No product behaviour changes. No public Rust API, config, hook, middleware, ACP or provider wire shape, Extism contract, session DB schema, Python binding or typeshare type changes. Default builds, CI and release artefacts are unchanged.

## Non-goals

- Patching code outside the binary crate (`querymt-agent-session-core`, gpui-kit). subsecond patches only the tip crate.
- Splitting `querymt-gpui` into a library and a binary. That would defeat subsecond's patching.
- Hot-patching the wasm/web build (`dx serve --platform web`).
- Re-instancing state on struct layout changes, or adding `subsecond::call` boundaries beyond GPUI's render boundary.
- Adding the Dioxus CLI to `flake.nix` or CI.
- A cargo alias for hot-patching: `dx` is not a cargo subcommand, so the script is the entry point.
- A UI design playground or component gallery window.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None. This is opt-in developer tooling (a cargo feature, a script and docs) with no behaviour change in default or release builds, so the change sets `skip_specs: true`.

## Impact

- **Packages:** `querymt-gpui` only. The new feature `hot-patch` adds no Rust code in this crate; it only enables gpui-pre's `hot-reload` (`gpui_hot_reload` cfg) in debug native builds.
- **Dependencies:**
  - a new optional direct dependency `gpui-pre = "=0.3.14"` (native targets only), the same crate and version gpui-kit already builds;
  - with the feature on, `subsecond`, `dioxus-cli-config`, `dioxus-devtools-types` and `async-tungstenite` (and their dependencies) enter the build;
  - `Cargo.lock` gains these packages even though the feature is off by default.
  - `querymt-gpui` is in hakari `traversal-excludes`, so `querymt-workspace-hack` is unaffected.
- **Tools:** the Dioxus CLI 0.7.10 (`cargo binstall dioxus-cli@0.7.10`), needed only for hot-patching.
- **Repository files:**
  - `crates/querymt-gpui/Cargo.toml` (feature and optional dependency);
  - new `crates/querymt-gpui/scripts/hot-patch.sh`;
  - `crates/querymt-gpui/knowledge/development.md`.
- **Special builds:** none affected. wasm32 builds never include the dependency (target-gated). Release profiles have no debug assertions, so gpui-pre compiles the patch path out.
- **CI:** unchanged. `cargo t-gpui` and `cargo cl-gpui` build without the feature. One extra local check builds with `--features hot-patch`.
- **querymt-desktop:** no UI follow-up.
