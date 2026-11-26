---
id: gpui-development
title: Building, testing and changing querymt-gpui
description: Build targets (native, wasm, Nix), where tests live and how CI runs them, golden files, the in-workspace session core, fonts, release packaging and the OpenSpec change workflow.
tags: build, test, ci, nextest, clippy, wasm, web, nix, golden, fonts, session core, release, packaging, openspec, workflow
knowledge_paths: ["crates/querymt-gpui/Cargo.toml", "crates/querymt-gpui/build.rs", "crates/querymt-gpui/web/build.sh", ".github/workflows/test-gpui.yml"]
knowledge_sources: ["crates/querymt-gpui/Cargo.toml", ".github/workflows/test-gpui.yml", "crates/querymt-gpui/web/build.sh"]
---

# Building, testing and changing querymt-gpui

The app lives in the QueryMT workspace as `crates/querymt-gpui` (package `querymt-gpui`).
Paths in these notes and in the GPUI OpenSpec changes (`src/...`, `tests/...`, `web/...`)
are relative to that directory unless they start with `crates/`.

## One binary crate

There is no library target: `src/main.rs` is the crate root for both the native binary and
the wasm32 build. All unit and GPUI tests live in the **bin target** (`#[cfg(test)]` modules,
some in separate files such as `src/quick/tests.rs`), so test commands need `--bins`.
Errors are plain `String`s (no `anyhow`/`thiserror`). Prefer `pub(crate)` for new APIs;
older code still has many plain `pub` items.

The crate is a workspace member but not a default member: plain `cargo build` at the
repository root does not build it (it needs GPUI's system libraries on Linux). Always pass
`-p querymt-gpui`.

## Commands (from the repository root)

```bash
nix develop -c cargo run -p querymt-gpui                              # native; qmtcode on PATH becomes "Local qmtcode"
nix develop -c cargo run -p querymt-gpui -- --agent 127.0.0.1:42069   # add a WebSocket agent (also QMT_AGENT)
QMT_DEBUG=1 nix develop -c cargo run -p querymt-gpui                  # agent/RPC traffic to stderr
cargo t-gpui                                                          # tests, as CI runs them (nextest --bins)
cargo cl-gpui                                                         # clippy -D warnings
cargo fmt --all -- --check
nix develop -c crates/querymt-gpui/web/build.sh [--release]           # wasm into crates/querymt-gpui/dist
```

CI (`.github/workflows/test-gpui.yml`): clippy with `-D warnings`, then
`cargo llvm-cov nextest -p querymt-gpui --bins --locked` on Ubuntu, and the release web
build. It runs when the crate, `crates/agent-session-core`, `crates/agent-protocol-types`
or the workspace manifests change. Formatting and the workspace-hack check are in
`lint.yml`. Code must compile on both native and `wasm32-unknown-unknown`: native-only
code (quick panel, processes, sockets, tray) is gated with
`#[cfg(not(target_family = "wasm"))]`.

## Web build

`web/build.sh` builds with `RUSTC_BOOTSTRAP=1` (the `wasm_thread` dependency uses a nightly
feature gate; the app still runs single-threaded), then `wasm-bindgen` and, for release
(`--profile release-gpui-web`), `wasm-opt`. The `wasm-bindgen` CLI in the root `flake.nix`
and in the CI workflows must match the `wasm-bindgen` version pinned in the workspace
`Cargo.lock`. Icons used on the web must be listed in `icon_assets!` in `src/main.rs`.

## Fonts

`build.rs` downloads five IBM Plex fonts from a pinned commit, verifies SHA-256 and caches
them in the OS cache dir (`QMT_FONT_CACHE` overrides). `CARGO_NET_OFFLINE=true` refuses to
download; the first uncached build needs network. The Nix build pre-fetches them.

## Golden session files

`src/session/golden.rs` compares projected transcript rows with `tests/golden/session/`.
A deliberate change of what rows show: rerun with `UPDATE_GOLDEN=1` and review the diff.
The same test replays the session core's seeded golden cases
(`querymt_agent_session_core::testing::golden_cases()`), so a change to the core that alters
rows shows up here in the same pull request.

## Session core

`querymt-agent-session-core` is a path dependency (`crates/agent-session-core`). There is no
pin to bump: a change to the core or to `querymt-agent-protocol-types` must keep
`cargo t-gpui` green.

## Releases

The app ships with the main QueryMT release and nightly (`.github/workflows/build.yml`) as
`QueryMT-Desktop-<version>-<platform>` bundles plus `querymt-desktop-web-<version>.tar.gz`.
The version is the workspace version; CI passes the exact release or nightly version as
`QMT_VERSION`, which the app reports instead of `CARGO_PKG_VERSION` (`util::app_version`).
Native release builds use the `release-gpui` profile (size-optimized, fat LTO, abort on
panic) and are packaged by `scripts/package/{macos,windows,linux}.sh`.

## Changing behaviour: OpenSpec

Non-trivial changes go through OpenSpec in the repository root (`openspec/`, skills
`openspec-propose`, `openspec-apply-change`, `openspec-archive-change`):

- `openspec/specs/<capability>/spec.md`: accepted behaviour.
- `openspec/changes/<change>/`: `proposal.md`, `design.md` (numbered decisions D1, D2, …
  that module docs cite; every change starts at D1, so name the change when citing one
  that is not the file's usual one, see [README.md](README.md)), `tasks.md`, and delta
  specs.
- Archiving moves a change to `openspec/changes/archive/<date>-<change>/` and merges its
  delta specs into `openspec/specs/`.

## Repository knowledge

These notes are indexed by QueryMT's repository knowledge (the root `.qmt/knowledge.toml`).
After moving files or rewriting a note, run the check:

```bash
cargo run -p querymt-agent --example repository_knowledge -- --root . check
```
