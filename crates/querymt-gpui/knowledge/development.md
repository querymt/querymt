---
id: gpui-development
title: Building, testing and changing querymt-gpui
description: Build targets (native, wasm, Nix), where tests live and how CI runs them, golden files, bumping the pinned session core, fonts, and the OpenSpec change workflow.
tags: build, test, ci, nextest, clippy, wasm, web, nix, golden, fonts, session core pin, openspec, workflow
knowledge_paths: ["Cargo.toml", "build.rs", "flake.nix", "web/build.sh", ".github/workflows/test.yml"]
knowledge_sources: ["Cargo.toml", ".github/workflows/test.yml", "web/build.sh"]
---

# Building, testing and changing querymt-gpui

## One binary crate

There is no library target: `src/main.rs` is the crate root for both the native binary and
the wasm32 build. All unit and GPUI tests live in the **bin target** (`#[cfg(test)]` modules,
some in separate files such as `src/quick/tests.rs`), so test commands need `--bins`.
Errors are plain `String`s (no `anyhow`/`thiserror`). Prefer `pub(crate)` for new APIs;
older code still has many plain `pub` items.

## Commands

```bash
nix develop -c cargo run                              # native; qmtcode on PATH becomes "Local qmtcode"
nix develop -c cargo run -- --agent 127.0.0.1:42069   # add a WebSocket agent (also QMT_AGENT)
QMT_DEBUG=1 nix develop -c cargo run                  # agent/RPC traffic to stderr
cargo nextest run --bins                              # tests, as CI runs them
cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings
cargo fmt --all -- --check
nix develop -c ./web/build.sh [--release]             # wasm into ./dist
```

CI (`.github/workflows/test.yml`): fmt, clippy with `-D warnings`, then
`cargo llvm-cov nextest --package querymt-gpui --bins --locked` on Ubuntu. Code must
compile on both native and `wasm32-unknown-unknown`: native-only code (quick panel,
processes, sockets, tray) is gated with `#[cfg(not(target_family = "wasm"))]`.

## Web build

`web/build.sh` builds with `RUSTC_BOOTSTRAP=1` (the `wasm_thread` dependency uses a nightly
feature gate; the app still runs single-threaded), then `wasm-bindgen` and, for release,
`wasm-opt`. The `wasm-bindgen` CLI in `flake.nix` must match the `wasm-bindgen` version
pinned in `Cargo.lock`. Icons used on the web must be listed in `icon_assets!` in
`src/main.rs`.

## Fonts

`build.rs` downloads five IBM Plex fonts from a pinned commit, verifies SHA-256 and caches
them in the OS cache dir (`QMT_FONT_CACHE` overrides). `CARGO_NET_OFFLINE=true` refuses to
download; the first uncached build needs network. The Nix build pre-fetches them.

## Golden session files

`src/session/golden.rs` compares projected transcript rows with `tests/golden/session/`.
A deliberate change of what rows show: rerun with `UPDATE_GOLDEN=1` and review the diff.

## Bumping the session core

`querymt-agent-session-core` is a git dependency pinned by `rev` (see the comment in
`Cargo.toml`). A bump changes the `rev` in `[dependencies]` and `[dev-dependencies]`
together, refreshes `cargoLock.outputHashes` in `flake.nix`, and runs the tests (they read
the core's golden cases from the pinned rev). For local work against a checkout, use an
uncommitted `[patch."https://github.com/querymt/querymt"]` pointing at
`../querymt/crates/agent-session-core`.

## Changing behaviour: OpenSpec

Non-trivial changes go through OpenSpec (`openspec/`, skills `openspec-propose`,
`openspec-apply-change`, `openspec-archive-change` in `.agents/skills/`):

- `openspec/specs/<capability>/spec.md`: accepted behaviour.
- `openspec/changes/<change>/`: `proposal.md`, `design.md` (numbered decisions D1, D2, …
  that module docs cite; every change starts at D1, so name the change when citing one
  that is not the file's usual one, see [README.md](README.md)), `tasks.md`, and delta
  specs.
- Archiving moves a change to `openspec/changes/archive/<date>-<change>/` and merges its
  delta specs into `openspec/specs/`.

## Repository knowledge

These notes are indexed by QueryMT's repository knowledge (`.qmt/knowledge.toml`). After
moving files or rewriting a note, run the check from a querymt checkout:

```bash
cargo run -p querymt-agent --example repository_knowledge -- --root <this repo> check
```
