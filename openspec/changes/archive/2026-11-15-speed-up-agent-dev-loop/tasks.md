# Tasks

This change sets `skip_specs: true` (tooling, docs and test layout only), so no task covers spec scenarios. Each task states its own verification. `AGENT_FEATURES` = `dashboard,oauth,zbus-secret-service,systemd-credentials,remote`; `RUNTIME_FEATURES` = `remote,oauth,plugin-loaders,api`. Every measurement records the date, `rustc -V`, the command and the probe file.

## 1. Environment and measurement method

- [x] 1.1 Switch to rustup:
  - The developer unlinks the Homebrew `rust` formula (`brew unlink rust`).
  - `AGENTS.md` (Development Environment) documents rustup as the non-Nix toolchain, with `rust-toolchain.toml` honoured.
  - Verify:
    - `which cargo` resolves to `/opt/homebrew/opt/rustup/bin/cargo`;
    - `cargo -V` and `rustc -V` match `rustup run stable rustc -V`;
    - `cargo +nightly -V` works;
    - one cold `cargo check -p querymt-agent-core` succeeds.
- [x] 1.2 Add `scripts/measure-agent-edit-loop.sh` (design D5).
  - Inputs: probe file, mode `shift|literal`, label, cargo command.
  - Output: one line with wall time, peak summed RSS, largest process and maximum process count.
  - Restores the probe file on exit (trap) and refuses a file with uncommitted changes.
  - Verify:
    - run it twice on `crates/agent-runtime/src/session/compaction.rs` with `cargo test -p querymt-agent-runtime --features RUNTIME_FEATURES --no-run`; both lines are printed;
    - `git status` is clean afterwards;
    - a run against a dirty file exits non-zero without editing it.
  - Result (2026-11-15, rustc 1.100.0, `RUSTC_WRAPPER=sccache`, `CARGO_INCREMENTAL` unset; `MEASURE_WARM=1` on the first run):
    - `runtime-shift-1`: wall=15.5s peak_rss=6.1GB max_proc_rss=3.9GB max_procs=4 exit=0
    - `runtime-shift-2`: wall=10.1s peak_rss=4.7GB max_proc_rss=3.2GB max_procs=4 exit=0
    - `git status --short crates/` is empty afterwards.
    - Dirty file (`AGENTS.md`, uncommitted edit): exit 2, "refusing to edit", checksum unchanged. Invalid mode: exit 2.
- [x] 1.3 Document incremental compilation (design D2) and add `scripts/clean-target.sh`.
  - `AGENTS.md` says `CARGO_INCREMENTAL` stays unset, and gives the measured cost of `=0` and the sccache conflict of `=1`.
  - The script prints `target/` size, removes `target/debug/incremental` (`--all`: `target/debug`), and prints the size again.
  - Verify:
    - with `CARGO_INCREMENTAL` unset, `scripts/measure-agent-edit-loop.sh` on the runtime probe with the facade command (`cargo test -p querymt-agent --features AGENT_FEATURES --lib --tests --no-run`) reports ≤ 16 s on the second and third edit;
    - `scripts/clean-target.sh` reduces `target/` size, and the next build succeeds.
  - Result (2026-11-15, rustc 1.100.0, probe `crates/agent-runtime/src/session/compaction.rs`, shift, `CARGO_INCREMENTAL` unset):
    - edit 1 (after warm-up): wall=17.2s peak_rss=17.1GB max_proc_rss=3.2GB max_procs=30
    - edit 2: wall=12.6s peak_rss=19.9GB max_proc_rss=3.1GB max_procs=32
    - edit 3: wall=12.6s peak_rss=20.3GB max_proc_rss=3.0GB max_procs=32
    - `scripts/clean-target.sh`: `target/` 98G -> 85G (incremental was 17G); the following `cargo test ... --no-run` succeeded.
- [x] 1.4 Measure parallelism (design D4).
  - Run the facade command after a runtime probe edit with `CARGO_BUILD_JOBS` unset, `8` and `6`, two runs each.
  - Record the numbers below this task. Update `AGENTS.md` with a recommendation only if D4's threshold is met.
  - Verify: six result lines are recorded, and the decision is stated.
  - Result (2026-11-15, rustc 1.100.0, probe `crates/agent-runtime/src/session/compaction.rs`, shift, facade command with `AGENT_FEATURES`, incremental cache warmed first):

    | `CARGO_BUILD_JOBS` | run | wall | peak summed RSS | largest process | max procs |
    |---|---|---|---|---|---|
    | unset | 1 | 15.0 s | 18.4 GB | 3.2 GB | 30 |
    | unset | 2 | 12.7 s | 20.1 GB | 3.1 GB | 32 |
    | 8 | 1 | 12.7 s | 19.7 GB | 3.1 GB | 32 |
    | 8 | 2 | 12.9 s | 19.9 GB | 2.4 GB | 32 |
    | 6 | 1 | 12.9 s | 16.3 GB | 3.2 GB | 24 |
    | 6 | 2 | 12.1 s | 14.2 GB | 3.0 GB | 24 |

  - Decision: D4 threshold (≥ 30% lower peak RSS for ≤ 15% more wall time) is not met. `-j 8` changes nothing (about 19.8 GB vs 19.3 GB unset), and `-j 6` lowers the mean peak by about 21% (15.3 GB vs 19.3 GB; 24% against the steadier unset run 2) with no wall-time cost. `AGENTS.md` gets no `-j` recommendation; the default stays. Max procs counts the whole process tree (cargo, rustc, linkers, build scripts, sccache), not only rustc.

## 2. Standard commands

- [x] 2.1 Add `.cargo/config.toml` with the aliases from design D3 (verified 2026-11-15: `cargo t-core` 271 passed; `cargo cl-core` and `cargo cl-runtime` clean with `-D warnings`; `cargo t-runtime -E 'test(agent_log_targets_keep_querymt_agent_prefix)'` 1 passed, 2419 skipped; `QMT_UI_DIST=/tmp/qmt-dashboard-ui cargo t-agent -E 'test(/websocket/)'` 50 passed, 476 skipped):
  - `t-agent`, `t-runtime`, `t-services`, `t-sqlite`, `t-knowledge`, `t-index`, `t-core`;
  - `cl-agent`, `cl-runtime`, `cl-services`, `cl-sqlite`, `cl-knowledge`, `cl-index`, `cl-core`;
  - a comment pointing to `.github/workflows/test-agent.yml`.
  - Verify:
    - `cargo t-core` and `cargo cl-core` run;
    - `cargo cl-runtime` passes with `-D warnings`;
    - `cargo t-runtime -E 'test(agent_log_targets_keep_querymt_agent_prefix)'` runs and passes 1 test;
    - `QMT_UI_DIST=/tmp/qmt-dashboard-ui cargo t-agent -E 'test(/websocket/)'` runs the facade WebSocket tests with `AGENT_FEATURES`.
- [x] 2.2 Keep the feature lists in sync (verified 2026-11-15: `rg` shows identical `runtime_features`/`features` lists in `.cargo/config.toml` and `test-agent.yml`; both YAML files parse with PyYAML).
  - Add a comment next to `features` and `runtime_features` in `.github/workflows/test-agent.yml` that names `.cargo/config.toml`.
  - Document the aliases and the "test the package you changed, build the facade before pushing" loop in `AGENTS.md` and `openspec/config.yaml`.
  - Verify: `rg -n "remote,oauth,plugin-loaders,api" .cargo/config.toml .github/workflows/test-agent.yml` and the `AGENT_FEATURES` equivalent return matching lists, and the YAML still parses.
- [x] 2.3 Measure the alias sequence:
  1. `cl-agent`
  2. `cl-runtime`
  3. `t-runtime --no-run`
  4. `t-services --no-run`
  5. `t-agent --no-run`
  6. `cl-agent`

  Run it from a clean target dir, counting third-party `Compiling` lines and wall time per step. Repeat the sequence once warm. This is the baseline for 3.3.
  - Verify: both passes are recorded below this task.
  - Result (2026-11-15, rustc 1.100.0, `RUSTC_WRAPPER=sccache`, `CARGO_INCREMENTAL` unset, `CARGO_TARGET_DIR=/tmp/qmt-seq-target` starting empty, `QMT_UI_DIST` set; script `/tmp/qmt-alias-seq.sh` counts `Compiling`/`Checking` lines, split by path under the repo; all steps rc=0). Cold wall times include sccache hits for dependencies, so a machine with an empty sccache would be slower.

    | Step | Cold wall | Cold units (workspace / third-party) | Warm wall | Warm units |
    |---|---|---|---|---|
    | 1 `cl-agent` | 113 s | 1038 (11 / 1027) | 1 s | 0 |
    | 2 `cl-runtime` | 77 s | 99 (10 / 89) | 1 s | 0 |
    | 3 `t-runtime --no-run` | 180 s | 990 (10 / 980) | 1 s | 0 |
    | 4 `t-services --no-run` | 49 s | 239 (8 / 231) | 1 s | 0 |
    | 5 `t-agent --no-run` | 150 s | 144 (11 / 133) | 2 s | 0 |
    | 6 `cl-agent` | 1 s | 0 | 0 s | 0 |
    | total | 570 s | 2510 (50 / 2460) | 5 s | 0 |

    Third-party recompiles are dominated by the first check build (1027 crates, step 1) and the first test build (980 crates, step 3, `cfg(test)` and dev-dependencies). Step 2 recompiles 89 crates (runtime features differ from the facade's) and step 4 231 (services dev-dependencies). The warm pass recompiles nothing: the combinations coexist. This is the baseline for 3.3: hakari would have to remove most of the 89 + 231 + 133 cross-command recompiles (steps 2, 4, 5) to reach D8's threshold; steps 1 and 3 are the unavoidable first check and first test build.

## 3. Diagnostics

- [x] 3.1 Find the cause of the 55 s services line-shift cost. Measure each case twice with the facade command:
  - (a) a line-shifting edit at the end of `crates/agent-services/src/session/pruning.rs`, so no later spans move;
  - (b) a line-shifting edit in a services file without tracing spans or generics;
  - (c) the same edit as in the original measurement, with `cargo check -p querymt-agent-runtime --features RUNTIME_FEATURES` instead of the facade test build;
  - (d) the runtime build with `-Zincremental-info` via `cargo +nightly rustc`, to see how many runtime work products are reused.

  Record the numbers and a one-paragraph conclusion. If it names a fix that fits this change (for example a cheap code-layout change), add it as a task in group 4; otherwise note it for the runtime-split change.
  - Verify: eight timings and the conclusion are recorded.
  - Result (2026-11-15, rustc 1.100.0, `CARGO_INCREMENTAL` unset, facade command with `AGENT_FEATURES` unless noted). The script was changed during 3.1 to warm up on the unmodified file by default and to touch the restored file, so that every probe is measured against the original and not against the previous probe; the first, un-warmed runs of this task (which mixed 12-55 s) are discarded. Numbers below are from the fixed script, two runs each:

    | Case | Probe | Run 1 | Run 2 |
    |---|---|---|---|
    | original | `pruning.rs` first fn (line 20), shift | 51.4 s | 54.6 s |
    | original | same, literal (no line shift) | 15.7 s | 13.7 s |
    | (a) | end of `pruning.rs` (line 675, before tests), shift | 14.5 s | 14.5 s |
    | (b) | `accounting/pricing.rs` (line 11), shift | 14.1 s | 15.2 s |
    | (c) | first fn of `pruning.rs`, shift, `cargo check -p querymt-agent-runtime --features RUNTIME_FEATURES` | 16.8 s | 17.0 s |
    | (d) | first fn of `pruning.rs`, `cargo +nightly rustc -p querymt-agent-runtime --lib -- -Zincremental-info`, shift | 30.4 s (31.5 s in another run) | - |
    | (d) | same, literal | 6.5 s | 6.2 s |

    Bisecting the shift position in `pruning.rs` (facade command, one run each): shift at line 20, 193, 244 and 255 costs 52-58 s; shift at 260, 270, 275, 279, 285, 324, 356, 370 and 667 costs 14-18 s. The boundary is line 257, `pub trait TokenEstimator`. Timeline of a costly run: services 1 s, runtime 26 s, facade 25 s. `-Zincremental-info` printed only dep-graph node statistics (5.86 M nodes warm, 5.92 M after the shift), no reuse counts, so (d) shows the cost (30 s against 6 s) but not the reused work products.

    Layout experiment (reverted, not part of this change yet): moving the 3-line `TokenEstimator` trait into its own module `session/token_estimator.rs` and re-exporting it from `pruning.rs` made a shift at line 19 cost 17 s and 16 s, and a shift at line 200 cost 17 s.

    Conclusion: the 55 s is not general to services edits. A line shift only costs 55 s when it moves the definition of `TokenEstimator`, the one services trait used as `Arc<dyn TokenEstimator>` in the runtime (`session/compaction.rs`, `delegation/summarizer.rs`); everything else, including other services files, shifts at 14-17 s. The shifted trait span invalidates the runtime's work and then the facade's, which then redo their full codegen (runtime 26 s, facade 25 s; the `-Zincremental-info` runtime lib alone drops from 30 s to 6 s without the shift). I did not determine the exact rustc query that is invalidated, so the mechanism is inferred from the bisect and the experiment, which agree. A cheap code-layout fix fits this change: task 4.4 moves the trait to its own file, so edits to `pruning.rs` stop shifting its span. The general version (other dyn-trait definitions in lower crates) goes to the runtime-split change.
- [x] 3.2 Measure a core edit: a line-shifting probe in a core function body (for example in `crates/agent-core/src/events.rs`), facade command, two runs.
  - Verify: the numbers are recorded and `AGENTS.md`'s loop guidance mentions the core cost.
  - Result (2026-11-15, probe `crates/agent-core/src/events.rs` first function, facade command, warm-up on the original first): shift 18.3 s and 19.0 s (peak RSS 19.5 / 18.2 GB, 30 processes); literal 17.5 s and 16.4 s. A core edit costs 16-19 s whether or not lines shift, slightly above the 16 s goal, because every crate above core recompiles its changed dependency metadata. Mentioned in `AGENTS.md`.
- [x] 3.3 Measure `cargo hakari`.
  - Work in a scratch worktree (`git worktree add /tmp/qmt-hakari`). Install `cargo-hakari` there only, generate a workspace-hack limited to the agent stack, and rerun the 2.3 sequence from a clean target dir.
  - Record the recompiles and wall time against 2.3. Remove the worktree afterwards.
  - Verify: the comparison is recorded and the D8 decision is stated.
  - Result (2026-11-15, rustc 1.100.0, cargo-hakari 0.9.39 installed with `--root /tmp/hakari-root`, detached worktree at `e6d57852`, `platforms = ["aarch64-apple-darwin"]`, `[traversal-excludes]` listing all providers, the Python crates, the mobile FFI, `querymt-extism-macros`, `querymt-provider-common` and `querymt-cli`; `cargo hakari verify` passed; empty `CARGO_TARGET_DIR`; same sccache and `QMT_UI_DIST` as 2.3). `manage-deps` added the workspace-hack dependency to 11 crates: the 7 agent crates, `querymt`, `querymt-remote`, `querymt-utils` and `querymt-service-client`; 215 dependency lines were generated. Worktree and scratch target dirs were removed afterwards.

    | Step | 2.3 wall | 2.3 third-party units | hakari wall | hakari third-party units |
    |---|---|---|---|---|
    | 1 `cl-agent` | 113 s | 1027 | 109 s | 1036 |
    | 2 `cl-runtime` | 77 s | 89 | 46 s | 0 |
    | 3 `t-runtime --no-run` | 180 s | 980 | 171 s | 995 |
    | 4 `t-services --no-run` | 49 s | 231 | 41 s | 56 |
    | 5 `t-agent --no-run` | 150 s | 133 | 100 s | 19 |
    | 6 `cl-agent` | 1 s | 0 | 1 s | 0 |
    | total | 570 s | 2460 | 468 s | 2106 |

    Warm pass: 6 s, 0 units, in both. Recompiles in the cross-command steps 2, 4 and 5: 453 -> 75 third-party crates (-83%); wall time -102 s (-18%) over the sequence, with the first check and first test build (steps 1 and 3) unchanged.
  - Decision (D8): adopt. Both thresholds are met (≥ 50% fewer recompiled crates, ≥ 60 s saved). Caveats carried into 4.2: the saving is measured on macOS aarch64 only; `querymt` itself gets the workspace-hack dependency (it is shared by the wasm32-wasip1 providers), so 4.2 must check that `cargo build -p qmt-<name> --release --target wasm32-wasip1` still works and drop `querymt` from the managed crates if it does not; the 3.3 hakari config was saved to `/tmp/hakari-config.toml` as the starting point.

- [x] 3.4 Measure whether other `dyn` traits in lower crates behave like `TokenEstimator` (follow-up to 3.1, 2026-11-15). Method: `scripts/measure-agent-edit-loop.sh` got a `blank` mode (empty line after `PROBE_LINE`); for each trait one blank line just above its definition and one just after it, facade command with `AGENT_FEATURES`, warm-up on the original first, two runs each. Normal cost with no expensive shift is 10-14 s. The probes were restored (`git status` shows no probe leftovers).

  | Trait (defined in) | blank above | blank after |
  |---|---|---|
  | `EventJournal` (`agent-core/session/projection.rs:197`) | 68.4 / 66.3 s | 29.7 / 28.6 s |
  | `SessionStore` (`agent-core/session/store.rs:311`) | 61.1 / 54.2 s | 13.7 / 13.7 s |
  | `StorageBackend` (`agent-core/session/backend.rs:25`) | 56.5 / 54.1 s | 12.6 / 12.5 s |
  | `ScheduleRepository` (`agent-core/session/repository.rs:284`) | 54.4 / 55.3 s | 16.4 / 13.4 s |
  | `Tool` (`agent-services/tools/context.rs:440`) | 53.7 / 53.1 s | 10.1 / 11.0 s |
  | `ToolContext` (`agent-services/tools/context.rs:59`) | 53.8 / 53.3 s | 53.4 / 53.6 s |
  | `SnapshotBackend` (`agent-services/snapshot/backend.rs:131`) | 48.9 / 54.9 s | 13.4 / 11.0 s |
  | `ViewStore` (`agent-core/session/projection.rs:553`) | 25.4 / 26.1 s | 13.6 / 13.9 s |
  | `MiddlewareDriver` (`agent-runtime/middleware/driver.rs:48`) | 28.5 / 27.2 s | 10.9 / 11.0 s |
  | `AgentHandle` (`agent-runtime/agent/handle/mod.rs:86`) | 26.4 / 25.4 s | 38.8 / 36.9 s |
  | `AgentRegistry` (`agent-runtime/delegation/core.rs:109`) | 18.8 / 19.1 s | 18.2 / 19.9 s |

  Findings:
  - The `TokenEstimator` effect is general, not a one-off. A line shift above a `dyn`-used trait of a lower crate costs 50-68 s for every trait defined in `agent-core` or `agent-services` except `ViewStore` (25 s). The same shift just after the trait is back to 10-14 s, except where another trait sits below (`ToolContext` is followed by `Tool` in the same file; `EventJournal` is followed by `ViewStore`; `AgentHandle` and `AgentRegistry` have other items below).
  - The cost of a shift is that of the most expensive trait below the shift point in the same file. In practice: any edit that adds or removes lines above line 311 in `session/store.rs`, above line 440 in `tools/context.rs`, above line 553 (and especially above 197) in `session/projection.rs`, or in the top of `repository.rs`, `backend.rs` and `snapshot/backend.rs`, costs about 55 s; above `EventJournal` about 67 s. The upper traits (runtime, facade only above) cost 19-39 s.
  - Not measured: the mechanism (which rustc query is invalidated), edits that shift lines in files without such traits, and the `t-<pkg>` commands. The 3.2 core figure of 16-19 s used `events.rs`, which has no trait of this kind; a core edit in `store.rs` or `projection.rs` costs 55-68 s.
  - Consequence for 4.4: moving single traits into their own files fixes one trait per move; `SessionStore` and `EventJournal` live in 1011- and 1369-line files with 7-12 commits above them in 120 days, so the effect is hit regularly. Decision (user, 2026-11-15): apply the layout fix to all of these traits right away, runtime layer included, without a debug-info change and without first looking for the invalidated query. Done in group 6. The rustc-level cause (`-Zself-profile` with `summarize diff`) was not investigated.

## 4. Structural changes supported by group 3

- [x] 4.1 Merge the facade integration tests (design D7). Result (2026-11-15): the 7 files were moved with `git mv` into `crates/agent/tests/it/` (staged renames) and `it/main.rs` was added; the inner `#![cfg(feature = "remote")]` became `#[cfg(feature = "remote")] mod lan_mesh_capabilities;`. No `super`/`crate` paths needed fixing. Not committed: the working tree holds other uncommitted work, so the two commits in the plan are left to the developer. Checks: `cargo check -p querymt-agent --all-targets --features AGENT_FEATURES` passed; `cargo nextest run -p querymt-agent --features AGENT_FEATURES` 525 passed (2 leaky), 1 skipped (same as before); `--no-default-features --test it` builds and lists 0 `lan_mesh` tests (1 with `remote`); `cargo fmt -p querymt-agent -- --check` clean; `cargo clippy -p querymt-agent --all-targets --features AGENT_FEATURES -- -D warnings` clean. `add-portable-repository-knowledge/tasks.md` now uses `--test it -E 'test(/repository_knowledge_matrix/)'`. Measurement (runtime shift probe, facade command, warm-up first): wall 15.0 / 11.4 / 11.4 s, peak RSS 4.1 / 3.9 / 4.0 GB, largest process 3.1 / 2.6 / 3.2 GB, max processes 6 / 6 / 6, against 1.4's unset-`-j` runs of 15.0 / 12.7 s, 18.4 / 20.1 GB, 30 / 32 processes. Wall time is the same or better; processes and memory dropped by far more than the integration-test binaries alone would explain, so the 5x memory drop should be re-checked in 5.1 before it is quoted as a general figure.
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
- [x] 4.2 Apply design D8 per the 3.3 decision. Result (2026-11-15): adopted. `crates/workspace-hack` (`querymt-workspace-hack`, `publish = false`) is a workspace member and a dependency of the 7 agent crates, `querymt-remote`, `querymt-utils` and `querymt-service-client`; config in `.config/hakari.toml` (platforms `aarch64-apple-darwin` and `x86_64-unknown-linux-gnu`; providers, Python crates, mobile FFI, `querymt-extism-macros`, `querymt-provider-common` and `querymt-cli` excluded from traversal). Deviation from 3.3: `querymt` is in `[final-excludes]`, because with the dependency `cargo build -p qmt-openai --release --target wasm32-wasip1` failed in `aws-lc-sys` (it compiles on HEAD; the hack pulled host-only features into the wasm build). It still builds after the exclusion. `agent-core`, `querymt-remote` and `querymt-utils` get `[lints.cargo] unused_dependencies = "allow"`, because the hack crate trips Cargo's unused-dependency lint there. CI: `cargo hakari generate --diff`, `manage-deps --dry-run` and `verify` in the format job (`taiki-e/install-action@cargo-hakari`), plus path triggers for the new files; this CI step is not run locally. Checks: `cargo hakari verify` passes; 2.3 sequence in the main tree (empty target dir, hakari on): cold 106 / 47 / 175 / 41 / 100 / 2 s = 471 s with third-party units 1036 / 0 / 995 / 56 / 19 / 0, warm 0 units (matches the 3.3 scratch result: 75 cross-command recompiles against 453); `cargo fmt --all -- --check` clean; `cargo cl-<pkg>` clean for core, index, knowledge, sqlite, services, runtime, agent; `cargo check -p querymt-mobile-ffi` passes; `git status --short crates/providers` is empty. The Linux platform was added after those timings; `hakari verify`, a facade check and the wasm32 build were re-run and pass.
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
- [x] 4.4 Move `TokenEstimator` into its own file (finding of 3.1). Result (2026-11-15): `session/token_estimator.rs` added, `pub mod token_estimator;` in `session/mod.rs`, `pub use` in `pruning.rs`. `cargo cl-services` clean, `cargo t-services` 1006 passed 4 skipped, `cargo check -p querymt-agent-runtime --features RUNTIME_FEATURES` passes, fmt clean. A blank line inserted at the top of `pruning.rs` with the facade command now costs 13 s and 9 s (before: 51-55 s).
  - Create `crates/agent-services/src/session/token_estimator.rs` holding the trait, `pub mod token_estimator;` in `session/mod.rs`, and `pub use` it from `pruning.rs` so every existing path (`session::pruning::TokenEstimator`, `session::TokenEstimator`) keeps resolving.
  - Verify:
    - `cargo cl-services` and `cargo t-services` pass, `cargo check -p querymt-agent-runtime --features RUNTIME_FEATURES` passes;
    - the measurement script with a line shift at the top of `pruning.rs` and the facade command reports ≤ 20 s (two runs), against 51-55 s before;
    - fmt is clean.
- [x] 4.3 Re-count facade uses of runtime `test-support` items (`rg -c "test_utils::|test_helpers::|test_support::" crates/agent/src crates/agent/tests`). Result (2026-11-15): still 122 references in 22 files, all in `crates/agent/src` (none left in `tests/it`); the largest are `acp/websocket_tests.rs` (38), `runtime_tests/handle/remote_ext.rs` (15), `remote_session_actor_ref.rs` (10), `worktree_slash_commands.rs` (10). Too many to move in one session, so the dev-dependency stays and the reason is documented in `AGENTS.md` (D9).
  - If they can be removed in one session, move the helpers or tests to the runtime and drop `features = ["test-support"]` from the facade's `[dev-dependencies]`, then re-measure the 2.3 sequence.
  - Otherwise document in `AGENTS.md` why the runtime has a separate test-support variant (design D9).
  - Verify: either the dev-dependency is gone, with the facade nextest count unchanged and clippy clean, or the documented reason is in `AGENTS.md`.

## 5. Wrap-up

- [x] 5.1 Re-measure with the script (final table recorded in `design.md` Context, 2026-11-15; runtime 12.0/10.9 s, sqlite 10.7/9.8 s, services shift 11.2/10.9 s and literal 10.7/10.7 s, core 16.0/15.3 s, facade 5.0/5.0 s with the facade command, all within the 16 s goal except core at 15-16 s, which is at it; `pruning.rs` top shift 13/9 s from 4.4; `t-<pkg> --no-run` walls: runtime 17.6/16.1, sqlite 3.0, services 5.2, core 4.6, agent 16.5/15.3 s; the `t-runtime` loop is 16-18 s, slightly above the goal, as that command builds the runtime with its test profile):
  - runtime, sqlite, services (shift and literal), core and facade edits, using the facade command;
  - the per-package `t-<pkg> --no-run` loop for the edited package.

  Record a final table next to the design's Context table.
  - Verify: the table is recorded, and the runtime/sqlite/facade rows meet the ≤ 16 s goal or the gap is explained.
- [x] 5.2 Downstream and validation (ticked 2026-11-15 with one accepted known failure, see Status):
  - `cargo check -p querymt-mobile-ffi`;
  - `cargo check -p querymt-agent --no-default-features --lib`;
  - `cargo nextest run` for every agent package with its CI features;
  - typeshare `cmp` against `openspec/changes/split-agent-crate/baseline/types.ts`;
  - `openspec validate speed-up-agent-dev-loop --strict`.
  - Verify: all pass; the count matches the `split-agent-crate` total (4995 passed, 12 skipped).
  - Decision (user, 2026-11-15): the pre-existing `connect_budget_defaults_are_bounded_and_reason_specific` failure is accepted as a known failure and does not block this change. It fails identically on a clean HEAD; it was not fixed, weakened or skipped here. After group 6 the CI-style run was repeated: 4997 run, 4996 passed, 1 failed, 12 skipped.
  - Status (2026-11-15), before the decision:
    - passed: `cargo check -p querymt-mobile-ffi`; `cargo check -p querymt-agent --no-default-features --lib`; `scripts/generate-types.sh` output is byte-identical to `split-agent-crate/baseline/types.ts` (`cmp`); `openspec validate speed-up-agent-dev-loop --strict` valid.
    - nextest, CI-style (`cargo nextest run --no-fail-fast` over the 7 agent packages, `--lib --tests --features AGENT_FEATURES`): 4997 tests run, 4996 passed (1 leaky), 1 failed, 12 skipped.
    - failure: `querymt-agent-runtime agent::handle::remote_connect::tests::connect_budget_defaults_are_bounded_and_reason_specific` (`remote_connect.rs:1538`, assertion `ExplicitReconnect.connect_budget() > Open.connect_budget()`). Confirmed on a clean detached worktree at HEAD `e6d57852` (same panic), so it is not caused by this change. Not weakened or skipped; needs a decision (fix in the owning change or accept as known).
    - count: 4996 passed + 1 failed = 4997 against the recorded 4995 passed. The difference was not investigated; the recorded total may predate tests added since (e.g. in `add-portable-repository-knowledge`).
    - to finish: accepted as known (see Decision above).

## 6. Layout fix for `dyn` traits (finding of 3.4)

Rule: a trait that other crates use as `dyn` lives in its own file, and its old module re-exports it, so every existing path keeps resolving (same pattern as 4.4). No API, behaviour or debug-info change. Scope decided with the user: all traits of the 3.4 table, runtime layer included.

- [x] 6.1 `querymt-agent-core`: move `EventJournal` and `ViewStore` out of `session/projection.rs`, `SessionStore` out of `session/store.rs`, `ScheduleRepository` out of `session/repository.rs`, `StorageBackend` out of `session/backend.rs`, each into its own file under a directory of the same name, and re-export from the old module.
- [x] 6.2 `querymt-agent-services`: move `ToolContext` and `Tool` out of `tools/context.rs` and `SnapshotBackend` out of `snapshot/backend.rs`, same pattern.
- [x] 6.3 `querymt-agent-runtime`: move `AgentHandle` (`agent/handle/mod.rs`), `MiddlewareDriver` (`middleware/driver.rs`) and `AgentRegistry` (`delegation/core.rs`), same pattern. For `AgentHandle`, also find which item below it makes a blank line after the trait cost 37-39 s (3.4).
- [x] 6.4 Verify and re-measure.
  - Per package: `cargo cl-<pkg>`, `cargo t-<pkg>` (agent-core, agent-services, agent-runtime) and `cargo t-agent` with unchanged test counts; `cargo fmt --all -- --check`; `cargo check -p querymt-mobile-ffi`; typeshare `cmp` against `split-agent-crate/baseline/types.ts`; `cargo hakari verify`.
  - Re-run the 3.4 sweep (blank line at the top of each old file and just above each moved trait's new location, facade command, two runs). Goal: 10-16 s, against 50-68 s before. Record the table below this task, and explain any trait that stays above 16 s.
- [x] 6.5 Update `AGENTS.md`: replace the single-trait "about 55 s" sentence with the rule and its reason.
  Results (2026-11-15):
  - Moves (11 traits, each into its own private module with `pub use` in the old module, so all existing paths resolve; no API change): `agent-core/session/` gets `event_journal.rs`, `view_store.rs`, `session_store.rs`, `schedule_repository.rs`, `storage_backend.rs`; `agent-services/tools/` gets `tool_context.rs`, `tool.rs` and `snapshot/` gets `snapshot_backend.rs`; `agent-runtime` gets `middleware/middleware_driver.rs`, `delegation/agent_registry.rs`, `agent/handle/agent_handle.rs`. The old files lost 20-648 lines each. Done with a script (`/tmp/movetrait.py`, not kept) and `cargo fix` for unused imports; one import (`async_trait` in the `middleware/driver.rs` test module) had to be restored by hand.
  - `AgentHandle` (6.3): the item below the trait was not a single item. After the move a blank line anywhere in `handle/mod.rs` costs 10-11 s, so the 37-39 s were caused by the trait being in the same file as `LocalAgentHandle` and its types, which the move separated.
  - Checks: `cargo fmt --all -- --check` clean; `cargo cl-<pkg>` clean for core, index, knowledge, sqlite, services, runtime, agent; `cargo hakari verify` passes; `cargo check -p querymt-mobile-ffi` and `cargo check -p querymt-agent --no-default-features --lib` pass; typeshare output byte-identical to `split-agent-crate/baseline/types.ts`; CI-style nextest over the 7 agent packages: 4997 tests run, 4996 passed, 1 failed, 12 skipped, same as before the move (the one failure is the pre-existing `connect_budget_defaults_are_bounded_and_reason_specific`, see 5.2).
  - Re-measurement (facade command, `AGENT_FEATURES`, warm-up first, two runs, blank line; `ALLOW_DIRTY=1` added to the script to probe files with uncommitted work, which are still restored byte-for-byte):

    | Old file (blank line at top / middle) | Before (3.4) | After |
    |---|---|---|
    | `session/projection.rs` | 66-68 s | 15.6 / 14.5 s ; 15.2 / 15.3 s |
    | `session/store.rs` | 54-61 s | 17.2 / 15.0 s ; 15.6 / 15.6 s |
    | `session/repository.rs` | 54-55 s | 15.1 / 13.9 s ; 15.6 / 14.1 s |
    | `session/backend.rs` | 54-57 s | 12.4 / 12.2 s ; 12.3 / 12.0 s |
    | `tools/context.rs` | 53-54 s | 11.2 / 11.2 s ; 11.0 / 13.2 s |
    | `snapshot/backend.rs` | 49-55 s | 10.4 / 10.2 s ; 10.2 / 10.1 s |
    | `middleware/driver.rs` | 27-29 s | 10.9 / 11.5 s ; 11.7 / 11.7 s |
    | `delegation/core.rs` | 19 s | 19.9 / 19.6 s (top) ; 11.8 / 12.0 s (middle) |
    | `agent/handle/mod.rs` | 25-39 s | 10.7 / 10.7 s ; 10.4 / 10.5 s |

    New files (blank line after the trait): `event_journal` 12.3 / 11.8, `view_store` 12.3 / 12.0, `session_store` 11.7 / 12.3, `schedule_repository` 12.3 / 12.3, `storage_backend` 11.7 / 12.8, `tool_context` 9.9 / 9.6, `tool` 10.2 / 10.5, `snapshot_backend` 9.3 / 9.3, `middleware_driver` 8.2 / 8.9, `agent_registry` 8.2 / 8.8, `agent_handle` 8.8 / 8.8 s.
  - Goal met (10-16 s) for every case except the top of `delegation/core.rs` (19.9 / 19.6 s). That file is 3900 lines with `AgentRegistry` already moved out, so the remaining cost is not a trait effect; a shift in its middle costs 12 s. Unexplained: why the top (line 2, the imports) costs 8 s more than the middle. Not investigated, and not part of the `dyn`-trait pattern.
  - Limits: editing a moved trait file itself (adding or changing a method) still invalidates everything above it, which is expected for an interface change; and only blank-line shifts were measured, not real edits to the trait files.


## Workflow follow-up

- Runtime split into parallel sibling crates (`remote`, `acp`, `dotagents`) and code-level compile-time work: a separate change, coordinated with `unify-agent-runtime`, using the 3.1 findings.
- Archive this change after `split-agent-crate` merges into `main`.
