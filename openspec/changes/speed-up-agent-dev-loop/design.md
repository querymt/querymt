# Design

## Context

See proposal.md – Why. The measurements behind this design were taken on `refact/split-agent` at `6e558d0a` (macOS, 14 cores, 48 GB, rustc 1.100.0, `RUSTC_WRAPPER=sccache`, `AGENT_FEATURES`, `QMT_UI_DIST` set). The command was `cargo test -p querymt-agent --features AGENT_FEATURES --lib --tests --no-run`. The edit was a one-line `let` in a function body. Single runs unless noted.

| Setup | Edit in | Wall |
|---|---|---|
| `CARGO_INCREMENTAL=0` | any agent crate | 75–79 s |
| incremental (unset) | runtime (`session/compaction.rs`) | 12–14 s |
| incremental | sqlite (`session/schema.rs`) | 13.5 s |
| incremental | facade (`runner.rs`) | 7.5 s |
| incremental | services (`session/pruning.rs`), lines shift | 55 s (2 runs) |
| incremental | services, no lines shift | 16 s (2 runs) |

Other observations that shape the decisions:

- **Runtime crate `-Ztime-passes`** (non-incremental, 57 s total):
  - metadata 20.6 s;
  - monomorphization collector 18.1 s;
  - borrow check 15.7 s;
  - type check 11.6 s;
  - coherence 6.8 s;
  - macro expansion 1.1 s;
  - link 0.18 s.
- **Memory:** the facade test build peaks at about 22 rustc/ld processes and 17–20 GB summed RSS (shared pages counted twice). The largest single rustc is 3–5 GB. Facade clippy after a runtime edit takes 6 s and 2–3 GB.
- **Switching package, feature or profile sets** recompiles third-party crates the first time each combination is built. Afterwards the combinations coexist (0.9 s to switch back). Examples:

  | Command | Cost |
  |---|---|
  | first `cargo test` after only clippy builds | 980 crates, 158 s |
  | `-p querymt-agent-services` test build | 230 crates |
  | `-p querymt-agent-runtime --features RF,test-support` | 88.6 s, one rustc at 5.2 GB |

- **Disk:** the main `target/` reached 228 GB; 920 of 945 crates had more than one artifact directory; the runtime alone had 23 variants (7.4 GB).
- **Toolchain:** `which -a cargo` resolves `/opt/homebrew/bin/cargo` (Homebrew `rust` formula, 1.100.0) before `/opt/homebrew/opt/rustup/bin/cargo`. `rust-toolchain.toml` (`channel = "stable"`) is therefore ignored, and `cargo +nightly` fails. CI uses `dtolnay/rust-toolchain@stable` (`.github/workflows/test-agent.yml`), and Nix reads `rust-toolchain.toml` (`flake.nix`, `rustToolchain`).
- **sccache:** with `RUSTC_WRAPPER=sccache`, `CARGO_INCREMENTAL=1` fails ("incremental compilation is prohibited"). Leaving it unset gives incremental builds for workspace crates, while sccache still caches dependencies.
- **Facade tests:** `crates/agent/tests/` holds 7 files (3,226 lines), each its own binary (3.6–4.3 s compile+link each, in parallel). `lan_mesh_capabilities.rs` is gated by `#![cfg(feature = "remote")]`. None of them sets env vars, changes cwd or uses process-global statics.
- **Facade use of runtime test-support items:** 22 files, 122 references to `test_utils`, `test_helpers` or `test_support` items in `crates/agent/src` (mostly `runtime_tests/` and `api/agent_tests.rs`) and `tests/dotagents_runtime_e2e_test.rs`.

## Goals / Non-Goals

**Goals:**
- A documented local edit loop of ≤ 16 s for runtime, sqlite and facade edits, and a stated cost for services and core edits.
- One reproducible measurement method, so every later number is comparable.
- Fewer third-party recompiles when switching between the standard agent commands.
- Lower peak memory for the facade test build, without slowing it noticeably.

**Non-Goals:**
- Fixing the 55 s services line-shift cost structurally (runtime split, code changes). This change only diagnoses it.
- CI build time.

## Decisions

### D1. rustup, not the Homebrew `rust` formula
Use the rustup proxies from `/opt/homebrew/opt/rustup/bin` and unlink the Homebrew `rust` formula (`brew unlink rust`, or uninstall it).

Reasons:
- `rust-toolchain.toml` becomes effective.
- `cargo +nightly` works for `-Ztime-passes` and `-Zself-profile`.
- Toolchain updates happen deliberately (`rustup update`) instead of as a side effect of `brew upgrade`, which forced cold rebuilds during `split-agent-crate` (1.99.1 → 1.100.0).

Alternatives considered:
- The Nix devShell: the project supports it, but the user chose rustup.
- Pinning an exact version in `rust-toolchain.toml`: out of scope, because CI tracks `stable`.

### D2. `CARGO_INCREMENTAL` stays unset
Documented in `AGENTS.md`, with the measured cost of `=0` and the sccache conflict of `=1`.

Disk growth is handled by a cleanup helper, `scripts/clean-target.sh`. It reports `target/` size and removes `target/debug/incremental` (and with `--all`, the whole `target/debug`). This is what an incremental-on workflow needs, and it doesn't add a dependency.

Alternative considered: `cargo sweep`, which is age- and toolchain-based and finer-grained but an extra tool to install. It is mentioned as optional.

### D3. Aliases in `.cargo/config.toml` that mirror the CI feature sets
The aliases are named after what they do, per package.

| Alias | Command |
|---|---|
| `t-agent` | `nextest run -p querymt-agent --features AGENT_FEATURES` |
| `t-runtime` | `nextest run -p querymt-agent-runtime --features RUNTIME_FEATURES` |
| `cl-agent` | `clippy -p querymt-agent --all-targets --features AGENT_FEATURES -- -D warnings` |
| `cl-runtime` | the same for runtime with `RUNTIME_FEATURES` |

`t-<pkg>` and `cl-<pkg>` follow the same pattern for the other agent packages (no features).

Feature lists are copied from `.github/workflows/test-agent.yml` (`features`, `runtime_features`). A comment in both files says to keep them in sync.

Cargo aliases can't set env vars, so `QMT_UI_DIST` is still required for `dashboard` builds. That is documented, not scripted.

Alternatives considered:
- A `justfile` or `xtask`: a new tool or crate for a handful of commands.
- `[env]` in `.cargo/config.toml` for `QMT_UI_DIST`: it would silently point CI-like local builds at a path that may not exist.

### D4. Parallelism default from measurement
The `-j` value is chosen from the 1.4 measurement. If `-j 8` (or `-j 6`) cuts peak summed RSS by ≥ 30% with ≤ 15% more wall time, `AGENTS.md` recommends `CARGO_BUILD_JOBS` for local runs. Otherwise the default stays and the numbers are recorded.

It is not put into `.cargo/config.toml` `[build] jobs`, because that would also throttle CI and other machines.

### D5. Measurement script
`scripts/measure-agent-edit-loop.sh` (bash, stdlib Python 3.9+ for timing and RSS polling, like `scripts/generate-types.sh` uses plain tools):

- applies a probe edit to a given file, in one of two modes: line-shifting, or literal-only with the same line count;
- runs a given cargo command;
- prints wall time, peak summed RSS, largest single process and maximum process count;
- reverts the probe;
- refuses to run on a dirty file and always restores it (trap).

This replaces the ad-hoc `/tmp` scripts used for the numbers above.

### D6. Diagnose before structural changes (group 3 before group 4)
Three questions decide group 4:
- why line shifts in services cost 55 s;
- what a core edit costs;
- whether `cargo hakari` pays off.

Each is measured with the D5 script, in a scratch worktree for hakari, so the main `target/` is not disturbed.

### D7. Merge the facade integration tests into one binary
Move `crates/agent/tests/*.rs` (renames only, one commit) into `crates/agent/tests/it/`, add `tests/it/main.rs` with one `mod` per file, and keep `#![cfg(feature = "remote")]` as `#[cfg(feature = "remote")] mod lan_mesh_capabilities;` (the inner attribute is removed in the glue commit).

Cargo auto-discovers `tests/it/main.rs` as the test target `it`.

The tests are safe to share a process: none of them sets env vars, changes cwd or uses globals. nextest still runs each test in its own process.

Done when:
- the test count is unchanged;
- the process count and peak memory of the facade test build go down;
- wall time is the same or better.

### D8. Workspace-hack crate only on evidence
If 3.3 shows that `cargo hakari` removes most third-party recompiles across the standard alias sequence (≥ 50% of the recompiled crates, or ≥ 60 s), add a `querymt-workspace-hack` crate. It is limited to the agent stack's dependency set, `publish = false`, and added as a dependency of the agent crates. CI gets a `cargo hakari verify` step.

It must not be added to provider crates, because their wasm32-wasip1 builds would inherit host-only features.

Otherwise, record the numbers and drop the idea.

### D9. Runtime `test-support` dev-dependency stays unless cheap to remove
Given 122 references in 22 facade files (mostly `runtime_tests/`, which sit in the facade because they need top-layer types), removing the dev-dependency would mean moving those tests or their helpers again. Task 4.3 re-counts after 4.1 and only proceeds if the remaining uses fit in one session. The expected outcome is "keep, and document it as the reason for the runtime's separate test-support variant".

## Risks / Trade-offs

- [Unlinking Homebrew `rust` breaks other projects that rely on it] → `brew unlink rust` is reversible (`brew link rust`), and the rustup proxies serve all projects through `rust-toolchain.toml` or the default `stable`.
- [Incremental on grows `target/` again] → `scripts/clean-target.sh` and a documented size check. `incremental/` was 36 GB before the first cleanup.
- [Aliases drift from CI feature lists] → comments in both files, and task 2.2 checks that they match.
- [Merged test binary changes nextest IDs] → filters in `.config/nextest.toml` match test names, not binaries. The one doc reference is updated.
- [Measurement noise from parallel work on the machine] → the script records date, rustc version and the command. Results are single runs unless noted, and conclusions need two runs.
- [hakari adds a generated crate that must be kept in sync] → only adopted on evidence (D8), and CI verifies it.

## Migration Plan

Local-only. Each group lands as its own local commit on `refact/split-agent`. Rollback is `git revert` per group, plus `brew link rust` for D1.
