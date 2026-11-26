# AGENTS.md

## Working Rules

- Keep changes minimal and focused; avoid drive-by refactors and unrelated cleanup.
- Preserve existing staged, unstaged and untracked work.
- Keep this file limited to durable, repository-wide guidance. Do not automatically
  append task-specific corrections.
- Never write to, migrate, replace or delete the user's existing session database.
  Use a fresh test database or an online backup made through a read-only source
  connection; all test writes stay in the test database.
- Never stop or restart user-run QueryMT services or pre-existing `qmtcode` processes.
  Use separate test instances and stop only processes created for the current test.

## Repository Overview

- Workspace members and toolchain are defined in `Cargo.toml` and `rust-toolchain.toml`.
- `querymt`: core library; `querymt-cli`: the `qmt` CLI.
- Agent packages, bottom to top (Cargo enforces the layering):
  - `querymt-agent-core`: plain types, events, config contracts, storage traits.
  - `querymt-agent-index`, `querymt-agent-knowledge`: code index and repository knowledge.
  - `querymt-agent-sqlite`: SQLite session and knowledge storage (the only `rusqlite` user).
  - `querymt-agent-services`: accounting, worktrees, snapshots, export, built-in tools.
  - `querymt-agent-runtime`: agent handle, session actors, ACP, mesh, auth, config, hooks.
    Features: `remote`, `oauth`, `plugin-loaders`, `api`, `test-support`.
  - `querymt-agent`: facade that re-exports the old `querymt_agent::…` paths, plus the
    runner, quorum, profiles, WebSocket server, examples and the embedded dashboard. Its
    features forward to the runtime.
- Test the package you changed. For facade or downstream changes (`querymt-agent-py`,
  `querymt-mobile-ffi`, examples) also test `querymt-agent`. Log targets carry the crate
  prefix (`querymt_agent_runtime::…`); `RUST_LOG=querymt_agent=debug` covers every agent crate.
- `querymt-remote`: shared mesh and remote-provider primitives.
- `querymt-service-client`: service OAuth and API client primitives, not a server.
- Provider crates live under `crates/providers/`.
- `querymt-gpui` (`crates/querymt-gpui`): QueryMT Desktop, the GPUI desktop and web
  client. One binary crate (tests live in the bin target, so use `--bins`) that talks
  to agents only over ACP and uses `querymt-agent-session-core` by path. It is not a
  default member and builds for `wasm32-unknown-unknown` too; Linux builds need GPUI's
  system libraries (devShell, or the apt list in `.github/workflows/test-gpui.yml`).
  A change to `querymt-agent-session-core` or `querymt-agent-protocol-types` must keep
  `cargo t-gpui` green. Its own notes are in `crates/querymt-gpui/knowledge/`.

## Development Environment

- Detect Nix first. When available, use the project devShell (`nix develop` or
  `nix develop -c <command>`); an active project direnv shell is sufficient.
- Without Nix, use the supported native toolchain and dependencies. Check `flake.nix`
  for supported Nix platforms; diagnose setup failures rather than silently
  switching toolchains or environments.
- Leave `CARGO_INCREMENTAL` unset for local work. `CARGO_INCREMENTAL=0` makes every
  agent-crate edit cost about 60 s more (75-79 s instead of 8-16 s for the facade test
  build), and `CARGO_INCREMENTAL=1` makes `RUSTC_WRAPPER=sccache` refuse to run
  ("incremental compilation is prohibited"). Unset gives incremental builds for
  workspace crates while sccache still caches dependencies. Incremental caches grow
  `target/`; run `scripts/clean-target.sh` (`--all` removes all of `target/debug`).
- `crates/workspace-hack` (`querymt-workspace-hack`) is managed by `cargo hakari`
  (config in `.config/hakari.toml`); it unifies third-party features across the agent
  stack so switching between the aliases below does not recompile dependencies. After
  changing dependencies or features, run `cargo hakari generate` and
  `cargo hakari manage-deps` (`cargo install cargo-hakari --locked`); the `Lint`
  workflow runs `cargo hakari verify` on every pull request. Provider crates and `querymt` must not depend on it (wasm32).
- `querymt-agent` has a dev-dependency on `querymt-agent-runtime` with `test-support`
  (122 references in 22 facade files, mostly `runtime_tests/` and `acp/websocket_tests.rs`,
  which sit in the facade because they need top-layer types). That is why the runtime is
  built in a second variant (with `test-support`) for facade tests; removing it means
  moving those tests or their helpers.
- Run the commands below from the repository root inside the selected environment.

## Validation

- After Rust edits, run `cargo check` for the smallest affected package and relevant
  features. Run `cargo fmt` only after that check passes.
- Use focused red/green tests where applicable: demonstrate the actual failure,
  then verify the fix. Each added case must protect a distinct required behavior
  or real regression; prefer extending existing tests and fixtures.
- Do not add tests solely to prove deleted functionality stays deleted, repeat
  implementation logic or constants, or multiply equivalent input combinations.
  Avoid brittle assertions on incidental values and unrelated lifecycle checks.
- Keep test scaffolding proportional to the change. Reuse existing checks before
  adding helpers, dependencies or frameworks; moving excess into another file
  is not simplification. Trim redundant cases without weakening the real regression.
- Start with package-scoped checks; replace `<package>` and `<test_filter>`:
  ```sh
  cargo check -p <package>
  cargo nextest run -p <package> <test_filter>
  cargo fmt -p <package> -- --check
  cargo clippy -p <package> --all-targets -- -D warnings
  ```
  Use `cargo nextest run`, not `cargo test`. nextest skips doctests, so run
  `cargo test -p <package> --doc` when doc examples change. If cargo-nextest is
  not installed, say so and fall back to `cargo test` with the same scope.
- Agent packages have aliases in `.cargo/config.toml` that use the CI feature sets, so
  runs share build artifacts: `cargo t-<pkg> [filter args]` (nextest) and
  `cargo cl-<pkg>` (clippy, `-D warnings`) for `agent`, `runtime`, `services`, `sqlite`,
  `knowledge`, `index` and `core`; for example `cargo t-runtime -E 'test(/compaction/)'`.
  `agent` uses `dashboard-server` (dashboard code, placeholder page), so it never builds
  the wasm UI and needs no `QMT_UI_DIST` (see Dashboard Boundary).
  Loop: test the package you changed with its `t-<pkg>` alias, and build the facade
  (`cargo t-agent --no-run`) before pushing. Measured edit cost with incremental on:
  runtime/sqlite/services/facade edits 5-16 s and core edits 15-19 s (everything above
  core rebuilds); use `scripts/measure-agent-edit-loop.sh` to reproduce the numbers.
- A trait that another crate uses as `dyn` (`SessionStore`, `EventJournal`, `Tool`,
  `AgentHandle`, `TokenEstimator`, ...) lives in its own file, re-exported from the old
  module. A line shift above such a trait costs 50-68 s in the facade build (measured on
  11 traits), against 10-16 s once it has its own file. Keep new `dyn` traits out of
  files with other frequently edited code, and do not add code above them in their file;
  editing the trait itself still rebuilds everything above it.
- CI (`.github/workflows/test-agent.yml`) tests only the agent packages a change
  reaches: `scripts/ci-agent-affected.sh` maps changed paths to packages through the
  dependency edges (sqlite is a dev-dependency of services and runtime, so a sqlite
  change retests both). Mirror that locally. Standalone builds run with the
  workspace-hack disabled (`cargo hakari disable`, never commit the result) so a
  package that forgets a feature it needs fails. `querymt-mobile-ffi` is not built
  in CI yet (deferred while it is under heavy development).
- Expand validation to affected consumers and feature combinations. Consult the
  relevant `.github/workflows/` file for CI commands/features and `.config/nextest.toml`
  for test timeouts.
- Whole-workspace checks are optional broader validation, not the default iteration
  loop. Native providers can require expensive builds or model downloads; inspect
  their workflows before selecting tests. Do not apply `--all-features` indiscriminately.
- Report checks actually run, failures and remaining gaps. Do not describe partial
  validation as complete. Documentation-only edits do not require a Rust build.

## Dashboard Boundary

- The `dashboard` feature embeds QueryMT Desktop web: `crates/agent/build.rs` builds
  `crates/querymt-gpui` for `wasm32-unknown-unknown` (profile `release-gpui-web`, in
  `target/querymt-gpui-web`). The app lives here and is changed in the same pull
  request when a backend change needs it. The Tauri `querymt-desktop` repository is
  separate; report required follow-up there instead of expanding into it.
- `dashboard-server` is the dashboard server code (routes, same-origin ACP guard,
  `--dashboard`) with a placeholder page from `crates/agent/dashboard-placeholder/`.
  Use it for backend checks and tests (`t-agent`/`cl-agent` do). `dashboard` adds the
  embedded UI; `QMT_UI_DIST` points it at a prebuilt UI directory instead of the wasm
  build. The real embed path is checked by `test-gpui.yml` (dashboard-embed).

## Code Conventions

- Follow surrounding code, rustfmt and existing error conventions.
- Use the surrounding crate's `log`/`tracing` conventions for diagnostics; preserve
  intentional CLI/protocol output. Include useful failure context without secrets.
- Implement backward compatibility only when explicitly requested by the user.
  Do not add compatibility layers or preserve legacy behavior by default. If a change
  appears to require backward compatibility, flag it for user review before implementing it.
- SQLite schema changes (`crates/agent-sqlite/src/session/schema.rs`) need a new migration in
  `crates/agent-sqlite/src/session/sqlite_storage/migrations.rs`; `init_schema` only covers fresh
  installs. Never edit a released migration. The schema drift test enforces this.
- Draw diagrams (flows, sequences, state machines, architecture) as ` ```mermaid `
  code blocks, never as ASCII art or box drawings. This applies to docs, OpenSpec
  artifacts and Markdown in general; `docs/mkdocs.yml` renders mermaid.
