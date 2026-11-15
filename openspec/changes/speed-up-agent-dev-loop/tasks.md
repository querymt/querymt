# Tasks

This change sets `skip_specs: true` (tooling, docs and test layout only), so no task covers spec scenarios. Each task states its own verification. `AGENT_FEATURES` = `dashboard,oauth,zbus-secret-service,systemd-credentials,remote`; `RUNTIME_FEATURES` = `remote,oauth,plugin-loaders,api`. Every measurement records the date, `rustc -V`, the command and the probe file.

## 1. Environment and measurement method

- [ ] 1.1 Switch to rustup:
  - The developer unlinks the Homebrew `rust` formula (`brew unlink rust`).
  - `AGENTS.md` (Development Environment) documents rustup as the non-Nix toolchain, with `rust-toolchain.toml` honoured.
  - Verify:
    - `which cargo` resolves to `/opt/homebrew/opt/rustup/bin/cargo`;
    - `cargo -V` and `rustc -V` match `rustup run stable rustc -V`;
    - `cargo +nightly -V` works;
    - one cold `cargo check -p querymt-agent-core` succeeds.
- [ ] 1.2 Add `scripts/measure-agent-edit-loop.sh` (design D5).
  - Inputs: probe file, mode `shift|literal`, label, cargo command.
  - Output: one line with wall time, peak summed RSS, largest process and maximum process count.
  - Restores the probe file on exit (trap) and refuses a file with uncommitted changes.
  - Verify:
    - run it twice on `crates/agent-runtime/src/session/compaction.rs` with `cargo test -p querymt-agent-runtime --features RUNTIME_FEATURES --no-run`; both lines are printed;
    - `git status` is clean afterwards;
    - a run against a dirty file exits non-zero without editing it.
- [ ] 1.3 Document incremental compilation (design D2) and add `scripts/clean-target.sh`.
  - `AGENTS.md` says `CARGO_INCREMENTAL` stays unset, and gives the measured cost of `=0` and the sccache conflict of `=1`.
  - The script prints `target/` size, removes `target/debug/incremental` (`--all`: `target/debug`), and prints the size again.
  - Verify:
    - with `CARGO_INCREMENTAL` unset, `scripts/measure-agent-edit-loop.sh` on the runtime probe with the facade command (`cargo test -p querymt-agent --features AGENT_FEATURES --lib --tests --no-run`) reports ≤ 16 s on the second and third edit;
    - `scripts/clean-target.sh` reduces `target/` size, and the next build succeeds.
- [ ] 1.4 Measure parallelism (design D4).
  - Run the facade command after a runtime probe edit with `CARGO_BUILD_JOBS` unset, `8` and `6`, two runs each.
  - Record the numbers below this task. Update `AGENTS.md` with a recommendation only if D4's threshold is met.
  - Verify: six result lines are recorded, and the decision is stated.

## 2. Standard commands

- [ ] 2.1 Add `.cargo/config.toml` with the aliases from design D3:
  - `t-agent`, `t-runtime`, `t-services`, `t-sqlite`, `t-knowledge`, `t-index`, `t-core`;
  - `cl-agent`, `cl-runtime`, `cl-services`, `cl-sqlite`, `cl-knowledge`, `cl-index`, `cl-core`;
  - a comment pointing to `.github/workflows/test-agent.yml`.
  - Verify:
    - `cargo t-core` and `cargo cl-core` run;
    - `cargo cl-runtime` passes with `-D warnings`;
    - `cargo t-runtime -E 'test(agent_log_targets_keep_querymt_agent_prefix)'` runs and passes 1 test;
    - `QMT_UI_DIST=/tmp/qmt-dashboard-ui cargo t-agent -E 'test(/websocket/)'` runs the facade WebSocket tests with `AGENT_FEATURES`.
- [ ] 2.2 Keep the feature lists in sync.
  - Add a comment next to `features` and `runtime_features` in `.github/workflows/test-agent.yml` that names `.cargo/config.toml`.
  - Document the aliases and the "test the package you changed, build the facade before pushing" loop in `AGENTS.md` and `openspec/config.yaml`.
  - Verify: `rg -n "remote,oauth,plugin-loaders,api" .cargo/config.toml .github/workflows/test-agent.yml` and the `AGENT_FEATURES` equivalent return matching lists, and the YAML still parses.
- [ ] 2.3 Measure the alias sequence:
  1. `cl-agent`
  2. `cl-runtime`
  3. `t-runtime --no-run`
  4. `t-services --no-run`
  5. `t-agent --no-run`
  6. `cl-agent`

  Run it from a clean target dir, counting third-party `Compiling` lines and wall time per step. Repeat the sequence once warm. This is the baseline for 3.3.
  - Verify: both passes are recorded below this task.

## 3. Diagnostics

- [ ] 3.1 Find the cause of the 55 s services line-shift cost. Measure each case twice with the facade command:
  - (a) a line-shifting edit at the end of `crates/agent-services/src/session/pruning.rs`, so no later spans move;
  - (b) a line-shifting edit in a services file without tracing spans or generics;
  - (c) the same edit as in the original measurement, with `cargo check -p querymt-agent-runtime --features RUNTIME_FEATURES` instead of the facade test build;
  - (d) the runtime build with `-Zincremental-info` via `cargo +nightly rustc`, to see how many runtime work products are reused.

  Record the numbers and a one-paragraph conclusion. If it names a fix that fits this change (for example a cheap code-layout change), add it as a task in group 4; otherwise note it for the runtime-split change.
  - Verify: eight timings and the conclusion are recorded.
- [ ] 3.2 Measure a core edit: a line-shifting probe in a core function body (for example in `crates/agent-core/src/events.rs`), facade command, two runs.
  - Verify: the numbers are recorded and `AGENTS.md`'s loop guidance mentions the core cost.
- [ ] 3.3 Measure `cargo hakari`.
  - Work in a scratch worktree (`git worktree add /tmp/qmt-hakari`). Install `cargo-hakari` there only, generate a workspace-hack limited to the agent stack, and rerun the 2.3 sequence from a clean target dir.
  - Record the recompiles and wall time against 2.3. Remove the worktree afterwards.
  - Verify: the comparison is recorded and the D8 decision is stated.

## 4. Structural changes supported by group 3

- [ ] 4.1 Merge the facade integration tests (design D7).
  - Commit 1 (renames only): `git mv crates/agent/tests/<file>.rs crates/agent/tests/it/<file>.rs` for the 7 files.
  - Commit 2 (glue):
    - add `tests/it/main.rs` with one `mod` per file and `#[cfg(feature = "remote")] mod lan_mesh_capabilities;`;
    - remove the inner `#![cfg(feature = "remote")]`;
    - fix any `super`/`crate` paths;
    - update `openspec/changes/add-portable-repository-knowledge/tasks.md` (`--test it -E 'test(/repository_knowledge_matrix/)'`).
  - Verify:
    - `cargo check -p querymt-agent --all-targets --features AGENT_FEATURES` passes;
    - `cargo nextest run -p querymt-agent --features AGENT_FEATURES` gives the same test count as before (525 passed, 1 skipped at the time of writing);
    - `cargo nextest run -p querymt-agent --no-default-features --test it` builds without the mesh test;
    - fmt and `cargo clippy -p querymt-agent --all-targets --features AGENT_FEATURES -- -D warnings` pass;
    - the measurement script shows fewer maximum processes and lower peak memory than 1.4's unset-`-j` runs, with wall time the same or better. Record the numbers.
- [ ] 4.2 Apply design D8 per the 3.3 decision.
  - If adopted:
    - add the workspace-hack crate (`publish = false`), add it to the agent crates and to the workspace;
    - add `cargo hakari verify` to `.github/workflows/test-agent.yml`;
    - check that the provider crates are untouched.
    - Verify:
      - `cargo hakari verify` passes;
      - the 2.3 sequence shows the measured reduction;
      - fmt and clippy pass for every agent package;
      - `cargo check -p querymt-mobile-ffi` passes;
      - `git diff --stat crates/providers` is empty.
  - If not adopted: record why, and tick the task.
- [ ] 4.3 Re-count facade uses of runtime `test-support` items (`rg -c "test_utils::|test_helpers::|test_support::" crates/agent/src crates/agent/tests`).
  - If they can be removed in one session, move the helpers or tests to the runtime and drop `features = ["test-support"]` from the facade's `[dev-dependencies]`, then re-measure the 2.3 sequence.
  - Otherwise document in `AGENTS.md` why the runtime has a separate test-support variant (design D9).
  - Verify: either the dev-dependency is gone, with the facade nextest count unchanged and clippy clean, or the documented reason is in `AGENTS.md`.

## 5. Wrap-up

- [ ] 5.1 Re-measure with the script:
  - runtime, sqlite, services (shift and literal), core and facade edits, using the facade command;
  - the per-package `t-<pkg> --no-run` loop for the edited package.

  Record a final table next to the design's Context table.
  - Verify: the table is recorded, and the runtime/sqlite/facade rows meet the ≤ 16 s goal or the gap is explained.
- [ ] 5.2 Downstream and validation:
  - `cargo check -p querymt-mobile-ffi`;
  - `cargo check -p querymt-agent --no-default-features --lib`;
  - `cargo nextest run` for every agent package with its CI features;
  - typeshare `cmp` against `openspec/changes/split-agent-crate/baseline/types.ts`;
  - `openspec validate speed-up-agent-dev-loop --strict`.
  - Verify: all pass; the count matches the `split-agent-crate` total (4995 passed, 12 skipped).

## Workflow follow-up

- Runtime split into parallel sibling crates (`remote`, `acp`, `dotagents`) and code-level compile-time work: a separate change, coordinated with `unify-agent-runtime`, using the 3.1 findings.
- Archive this change after `split-agent-crate` merges into `main`.
