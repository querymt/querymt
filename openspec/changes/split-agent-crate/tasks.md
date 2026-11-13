# Tasks

This change sets `skip_specs: true`, so no task covers a spec scenario. Every task proves "no behaviour change" with the existing tests, the test-count invariant (design D6) and the typeshare diff (design D9).

Shorthand used below:
- `AGENT_FEATURES` = `dashboard,oauth,zbus-secret-service,systemd-credentials,remote`, with `QMT_UI_DIST` set to the minimal fixture from `.github/workflows/test-agent.yml`, outside `target/`.
- `RUNTIME_FEATURES` = `remote,oauth,plugin-loaders,api`.
- Lower packages (core, index, knowledge, sqlite, services) have no features besides `test-support`.
- "Package checks" for a package means, in this order:
  1. `cargo check -p <pkg> --all-targets`
  2. `cargo nextest run -p <pkg> [--features …]`
  3. `cargo fmt -p <pkg> -- --check`
  4. `cargo clippy -p <pkg> --all-targets [--features …] -- -D warnings`
  5. `cargo test --doc -p <pkg>`
- "Count invariant" means the sum of `cargo nextest list --message-format oneline | wc -l` over every agent package, with the features above, equals the Phase 0 baseline.
- zbus and systemd paths only take effect on Linux. On macOS they stay unverified; record that.

## 0. Baseline and guard rails (can start now)

- [x] 0.1 Record the baseline on the current branch tip. Capture `cargo nextest list -p querymt-agent --features AGENT_FEATURES` (count and the sorted test-name list), typeshare output from `scripts/generate-types.sh` (stored outside `target/`), and a `--timings` report for one run: a one-line edit to a function body in `crates/agent/src/session/compaction.rs`. (Trimmed by agreement from three runs; the cold test build and clippy timings are retaken in 2.1 and 9.3.) Verify: the files are committed under `openspec/changes/split-agent-crate/baseline/` and the numbers are pasted below this task.
  - Baseline taken at `1c26c972` (0.4 applied) on macOS, `AGENT_FEATURES`, `QMT_UI_DIST=/tmp/qmt-dashboard-ui`, with `QUERYMT_REMOTE_CONNECT_OPEN_BUDGET_MS` unset.
  - Files in `baseline/`: `nextest-list.txt` (sorted `cargo nextest list -p querymt-agent --message-format oneline`), `types.ts` (output of `scripts/generate-types.sh`; two runs gave the same sha256 `258ff013...b175`), `timing-compaction-edit.html`.
  - Test count: 4345 (`nextest-list.txt`: 4345 lines); a full run is 4345 passed, 12 skipped.
  - typeshare: 2343 lines. Needs `~/.cargo/bin` on `PATH` (typeshare-cli 1.13.4).
  - Timing, one-line edit in `session/compaction.rs` (`cargo test -p querymt-agent --features AGENT_FEATURES --no-run --timings`, warm dependencies): 36.1 s wall (`user` 138 s). The `querymt_agent` lib (test) unit takes 35.6 s; each integration test and example then spends about 10 s linking against it.
  - Cold test build and clippy timings are retaken in 2.1 (see the trimmed scope above).
- [ ] 0.2 (deferred to group 2, by agreement; needed only before the Phase 3 in-place cuts. Phase 1 boundaries are enforced by Cargo.) Add `scripts/check-agent-layering.py`. It implements the file→layer map from design D1, the allowed edges, and the dev-only edges, and reports production violations per layer pair. It exits non-zero when the count is higher than the committed allowance in `scripts/agent-layering-allowance.json`. Verify: running it on the current tree reproduces the design D3 inventory to within ±10%, and a deliberately added upward `use` makes it fail.
- [ ] 0.3 (deferred to group 2, by agreement; needed only before the Phase 3 in-place cuts. Phase 1 boundaries are enforced by Cargo.) Add the layering check as a step in `.github/workflows/test-agent.yml`, before clippy. Verify: the workflow passes on a PR that only adds the check.
- [x] 0.4 Replace `env!("QMT_BUILD_VERSION")` with `querymt_utils::BUILD_VERSION` in `mcp_auth.rs`, `agent/mcp.rs`, `agent/handle/send_agent_lifecycle.rs` and `export/atif.rs`, and remove `emit_build_version` from `crates/agent/build.rs`. Verify: run the package checks for `querymt-agent` with `AGENT_FEATURES`, and check that a built `qmtcode --version` prints the same string as before.
  - Also replaced the two uses in `examples/qmtcode.rs` (the example is built by the flake and would otherwise stop compiling once `build.rs` no longer emits the variable).
  - Verified on macOS with `AGENT_FEATURES` and `QMT_UI_DIST=/tmp/qmt-dashboard-ui`: `cargo check --all-targets` ok; `cargo nextest run -p querymt-agent --no-fail-fast` 4345 passed, 12 skipped; `cargo fmt -- --check` ok; `cargo clippy --all-targets -- -D warnings` ok; `cargo test --doc` 20 passed, 6 ignored.
  - `qmtcode --version` prints `qmtcode nightly-188-gea5b061a-dirty`, equal to `git describe --tags --always --dirty`, which is what the removed `build.rs` code produced (`querymt-utils/build.rs` uses the same logic).
  - zbus and systemd paths are unverified (macOS).
  - Note: a shell with `QUERYMT_REMOTE_CONNECT_OPEN_BUDGET_MS` set makes `connect_budget_defaults_are_bounded_and_reason_specific` fail, because the test guards only the open override, not the reconnect comparison. Unrelated to this change; unset it before running the suite.

## 1. Extract `querymt-agent-core` (seed), `querymt-agent-index` and the knowledge crate (can start now)

- [x] 1.1 Create `crates/agent-core` (`querymt-agent-core`) holding only `hash.rs` and `DiffPaths`, which moves from `index/merkle.rs` with `merkle` re-exporting it. Add `pub(crate) use querymt_agent_core::hash;` aliases in `querymt-agent` and add `crates/agent-core` to the workspace members. Verify: package checks for `querymt-agent-core`.
  - `hash.rs` moved with `git mv`; `querymt-agent` has `pub use querymt_agent_core::hash;` in `lib.rs` (so `querymt_agent::hash` and `crate::hash` both still resolve), `index::merkle` re-exports `DiffPaths`. The `hash.rs` doctest now names `querymt_agent_core::hash::RapidHash`.
  - `rapidhash` stays a dependency of `querymt-agent` because `index/function_index/fingerprint.rs` calls it directly; it leaves with the index in 1.2.
  - Verified: core package checks (check, nextest 5 passed, fmt, clippy `-D warnings`, doc 1 passed); `cargo check -p querymt-agent --all-targets --features AGENT_FEATURES` ok; count invariant: agent 4340 + core 5 = 4345 = baseline.
- [ ] 1.2 Create `crates/agent-index` (`querymt-agent-index`) with `git mv crates/agent/src/index`. The move commit must contain renames only. Move every `tree-sitter*`, `similarity-*` and `syn` dependency to the index package, since nothing outside `index/` uses them. `ignore`, `notify` and `gix` stay in `querymt-agent` too because `snapshot`, `tools`, `skills`, `profiles` and `worktree` use them; the index declares its own copies. Add `insta`, `tempfile` and `tokio` as index dev-dependencies. `index/` has 5 `pub(crate)` items and all are used only inside it, so no visibility changes are needed. Add the alias `pub use querymt_agent_index as index;` in `querymt-agent`. Verify: package checks for `querymt-agent-index`; `cargo check -p querymt-agent --all-targets --features AGENT_FEATURES`; `git show --stat` of the move commit shows 100% renames.
- [ ] 1.3 Move the index-only `#[typeshare]` type (`index/file_index.rs`) under the new path in `scripts/generate-types.sh`. Verify: the typeshare output is byte-identical to the 0.1 baseline.
- [ ] 1.4 Update the CI path filters in `test-agent.yml` and `test-python-packages.yml` to `crates/agent*/**`. Add per-package clippy and nextest steps for `querymt-agent-core` and `querymt-agent-index`, and add them to the coverage `--package` list. Verify: CI passes, and the coverage report includes the index files.
- [ ] 1.5 Run the count invariant, then repeat the three 0.1 timing runs and record them below this task. Run it after 1.9 as well. Verify: the count equals the baseline, and the timings are recorded.
- [ ] 1.6 Move `HookContextContribution` from `hooks/engine.rs` into `crates/agent-core`, with `hooks::engine` re-exporting it. Verify: package checks for `querymt-agent-core`; `cargo check -p querymt-agent --all-targets --features AGENT_FEATURES`. Rebase the hunk over `harden-agent-types-and-errors`, which also edits `hooks/engine.rs`.
- [ ] 1.7 Move the pure path helpers from `tools/path.rs` into core: `normalize_tool_path`, `resolve_arg_path`, `effective_root`, `lexical_clean`, `pop_parent`, `canonicalize_existing_prefix` and `paths_overlap`. `tools::path` re-exports them, and the `FileAccess` and `ReadScope` functions stay. Verify: the `tools/path.rs` tests pass in their new location with the same names; package checks for core.
- [ ] 1.8 Move the dotagents document parsing into core: `dotagents/{frontmatter,layer,diagnostics}.rs`, `DotagentsMemory` from `manifest.rs`, and the memory-document part of `parsers.rs` (`parse_memory_document`, `partition_fields`, `fingerprint`, `MEMORY_KEYS`). Split `parsers.rs` rather than moving it, since the rest depends on `subagent` and `DotagentsManifest`. Core gains `serde_json` and `sha2`. `dotagents` re-exports every moved item at its old path. Verify: dotagents tests pass unchanged (`dotagents_manifest_preview_test`, `dotagents_runtime_e2e_test` included); the layering script shows no core → upper violations for these files.
- [ ] 1.9 (owner: the user, done on the `feat/kb` branch; `0001-wip-1.1.patch` in the working tree belongs here) Land `feat/kb` already split. Once 1.2 and 1.6 to 1.8 are on `main`, rebase `feat/kb` and:
  - `git mv crates/agent/src/knowledge/repository` to `crates/agent-knowledge/src/` (renames only);
  - move `config/repository_knowledge.rs` into the knowledge crate with explicit imports (it uses `use super::*` today), and have `config` re-export `RepositoryKnowledgeSettings`, `RepositoryKnowledgeDeliverySettings` and the `REPOSITORY_KNOWLEDGE_*` constants;
  - in the glue commit, give `querymt-agent-knowledge` dependencies on core and index, plus `rusqlite` (for `RepositoryStore`), `glob`, `ignore`, `sha2`, `thiserror`, `serde`, `schemars`, `serde_json`, `unicode-normalization` and `tempfile` (dev). No dependency on sqlite, services or runtime;
  - keep `knowledge::repository` as an alias for the new crate in `querymt-agent`, so `tools/builtins/repo_knowledge.rs`, `agent/*`, `api/*`, the example and `tests/repository_knowledge_matrix.rs` stay unchanged.
  Verify: package checks for `querymt-agent-knowledge`; `cargo tree -p querymt-agent-knowledge -e normal` lists no `querymt-agent` crate except core and index; `cargo check -p querymt-agent --all-targets --features AGENT_FEATURES`; `config/tests.rs` repository-knowledge tests and the `JsonSchema` output for the settings types are unchanged; the count invariant holds including the kb tests; add the knowledge crate to `generate-types.sh` (nothing to generate), the CI per-package steps and the coverage list.

## 2. Gate: merges before the cuts

Tasks 0.2 and 0.3 (layering script and CI step) are done here, before 2.1, so they use the refreshed inventory. Rerun 0.4-style timing runs (cold test build, clippy) together with 2.1.

- [ ] 2.1 Confirm that `feat/kb` has merged split (task 1.9), that `speed-up-session-loading`, `ordered-session-events` and `add-git-worktree-isolation` are archived, and that `harden-agent-types-and-errors` is merged. (`fix/snapshot` is already merged and is not checked.) Rebase this change's branch onto `main`. Rerun 0.1 (test-name list, typeshare, timings) and 0.2 (inventory) on the new tip and update the baseline and allowance. Verify: the updated baseline files are committed, and the allowance matches the refreshed inventory.
- [ ] 2.2 Record that `support-mcp-skills` (20 of 21 tasks done) lands before Phase 6 and before `AgentMode` moves in 3.1. Ask the owner of `support-pi-lens-hooks` whether it lands before Phase 6 or starts after it, and record the decision below this task. Verify: both decisions are recorded.

## 3. Cut the core edges in place (in `querymt-agent`)

Each task moves the listed definitions inside `crates/agent/src` into a staging module, `src/core_staging/`, mirroring the future `querymt-agent-core` layout. Old paths re-export the moved items. After each task, run `scripts/check-agent-layering.py` with a lowered allowance, then the package checks for `querymt-agent` with `AGENT_FEATURES` and the count invariant.

- [ ] 3.1 Move the plain enums and structs: `AgentMode`, `agent/session_control.rs` (whole file), `delegation/model_overrides.rs` (whole file, with its `SessionStore` function), `worktree/domain.rs` (whole file), `VerificationSpec`, `DuplicateWarning`/`FunctionLocation`/`SimilarMatch`, `ElicitationAction`/`ElicitationResponse`, `QUERYMT_META_NAMESPACE` and `acp/protocol.rs`. Verify: the layering check shows no core→runtime/services violations for these names.
- [ ] 3.2 Move the accounting value types (`AttemptStart`, `AttemptSettlement`, `RequestAccounting` and their field types) and `BillingBasis`. Turn `SessionProvider::get_pricing` into the free function `accounting::pricing_for(provider, model)` and keep the method as a delegate. Verify: the layering check passes for the `events.rs`→accounting edges; the existing accounting tests pass.
- [ ] 3.3 Move `SessionRuntimeStatus`, `SessionRuntimePhase`, `RuntimeObservation`, `DelegationStage`, `DelegationActivity` and `DelegationUpdateNotification`. The projector stays put. Verify: `events.rs` has no references to the runtime layer.
- [ ] 3.4 Move the config leaf types (`McpServerConfig`, the execution configs, `WorkspaceSettings`, `ApprovalNeed`; the repository-knowledge settings already moved in 1.9). Methods that take runtime types become free functions in `config`. Verify: `config/tests.rs` passes unchanged, including the `deny_unknown_fields` rejection tests; the `JsonSchema` output from `config::schema_for_value` is identical to the baseline (snapshot it in this task).
- [ ] 3.5 Move the pure helpers: `truncate_to_bytes`, `render_prompt_for_display`, `filter_to_effective_history`, `StreamCursor`/`observe_cursor` and `run_lease_root_for_db`. (`resolve_arg_path` and `effective_root` already moved in 1.7.) Verify: the layering check passes for `model.rs` and `session/store.rs`.
- [ ] 3.6 Split the files where a trait and its SQLite implementation share a file: `session/backend.rs` (the `impl StorageBackend for SqliteStorage` part), `session/repo_schedule.rs`, `worktree/repo_workspace.rs`, `dotagents/tasks.rs` and `dotagents/automation.rs`. Traits and record types go to staging; SQLite halves go to `src/sqlite_staging/`. Verify: no file in `core_staging/` mentions `rusqlite` or `Sqlite` (grep check in the layering script).
- [ ] 3.7 Move the knowledge store contract (`knowledge/mod.rs` minus `From<rusqlite::Error>`, `knowledge/text_processing.rs`). The dotagents document parsing already moved in 1.8. Verify: the layering check shows no knowledge-contract→runtime violations.
- [ ] 3.8 Remove `ViewStore::get_atif`. Add `export::session_atif(view_store, session_id, options)` built on `get_audit_view`, and update `api/agent_tests.rs` `SplitLifecycleBackend` and any callers. Verify: the existing `atif_*` tests in `export/` pass; add `session_atif_matches_previous_view_store_output`, which exports a fixture session and compares it with JSON captured from the old method before the removal.
- [ ] 3.9 Make `SessionError::Database` database-neutral (design D4.5). Replace `Database(#[from] rusqlite::Error)` with `Database(DbError)` where `DbError` holds a boxed `dyn Error + Send + Sync` source and a `DbErrorKind` (`NoRows`, `Busy`, `Other`). `is_no_rows()` and `is_transient()` read the kind; `db_message_contains()` reads the source text. Remove `From<rusqlite::Error>` for `SessionError` and `KnowledgeError`, add the `SqliteResultExt::db()` and `knowledge_db()` helpers in `sqlite_staging/`, and update the ~22 call-site files. Verify: grep shows no `From<rusqlite::Error>` impls and no `rusqlite` in core's `session/error.rs`; add `sqlite_error_classification_is_preserved`, which asserts that a busy error is `is_transient()`, `QueryReturnedNoRows` is `is_no_rows()`, a constraint-violation message round-trips through `db_message_contains()` with the same text as `rusqlite::Error::to_string()`, the display text is unchanged, and `source()` downcasts to `rusqlite::Error`; the existing `claim_delegation` retry tests still pass.
- [ ] 3.10 Split `test_utils`: put core mocks under `core_staging/test_support` and SQLite fixtures (`sqlite_conn_with_schema`, `TestStorage`) under `sqlite_staging/test_support`, both still `#[cfg(test)]`. Verify: the count invariant holds.

## 4. Extract `querymt-agent-core` (full)

- [ ] 4.1 `git mv` `core_staging/` into `crates/agent-core/src/` (renames only), then add the glue commit: `Cargo.toml` with only the dependencies core code uses (including `agent-client-protocol`, `rust-stemmers`, `unicode-normalization`, and `mockall` as optional), the `test-support` feature, aliases in `querymt-agent`, and `[workspace.dependencies]` entries for shared versions. Verify: package checks for `querymt-agent-core`; `cargo tree -p querymt-agent-core -e normal | grep -c rusqlite` prints 0.
- [ ] 4.2 Add core to `generate-types.sh`, the CI per-package steps and the coverage list. Verify: the typeshare output is identical to the baseline; the count invariant holds.

## 5. Extract `querymt-agent-sqlite` (parallel with 6)

- [ ] 5.1 Move `sqlite_staging/`, `session/sqlite_storage/`, `session/repo_*.rs`, `session/schema.rs`, `knowledge/sqlite.rs` and `tests/fixtures/schema_0001_baseline.sql` into staging, then `git mv` it to `crates/agent-sqlite`. Add the glue commit with a `test-support` feature exporting `TestStorage` and `sqlite_conn_with_schema`. `session/sqlite_storage/blocking.rs` moves with `sqlite_storage/`, and the `rusqlite`-based unit tests from `session/error.rs` move here. Tests in `session/sqlite_storage/tests.rs` and `session/repo_tests.rs` that reference services or runtime items move to services or runtime (design D6). Verify: package checks for `querymt-agent-sqlite`, including `legacy_baseline_upgrade_matches_fresh_install`; the count invariant holds.
- [ ] 5.2 Make `rusqlite` reachable in normal builds of the lower layers only through `querymt-agent-sqlite` and `querymt-agent-knowledge`. Verify: `cargo tree -e normal -i rusqlite` lists only those two packages and `querymt-agent` (the last one through the sqlite package). Record the output.
- [ ] 5.3 Open a session database created on `main` before this change with the new code. Verify: add `existing_database_opens_after_crate_split`, which copies a fixture database created by the Phase 0 build into a temp dir, opens it through `SqliteStorage::connect`, and reads sessions and events; it passes in `querymt-agent-sqlite`.

## 6. Knowledge crate follow-up (the extraction itself is task 1.9)

- [ ] 6.1 Confirm that nothing in Phases 2 to 5 added a dependency from `querymt-agent-knowledge` on core's storage contracts, sqlite, services or runtime. Verify: `cargo tree -p querymt-agent-knowledge -e normal` shows only core, index and external crates; `cargo tree -p querymt-agent-knowledge -e normal -i rusqlite` lists only the knowledge package's own direct dependency.

## 7. Extract `querymt-agent-services`

- [ ] 7.1 Replace `ToolContext::workspace_query_bridge` with a `WorkspaceQueryBridge` trait, moving `workspace_query.rs` with it, and implement it for `ClientBridgeSender`. Verify: the `language_query` tests pass, including the existing bridge-less case.
- [ ] 7.2 Replace `ToolContext::agent_registry` with `extensions()`, have runtime insert `Arc<dyn AgentRegistry>`, and update `delegate.rs`. Verify: the `delegate` tool tests `without_registry` and `with_registry` pass with unchanged error text.
- [ ] 7.3 Move the agent-aware built-ins `delegate.rs`, `route_delegation_to_peer.rs` and `use_remote_provider.rs` out of `tools/`. Runtime appends them to `all_builtin_tools()`, preserving the current order. Move `shell_executable`/`shell_exit_code` next to `shell.rs`, and split the projector out of `control/delegation_notifications.rs`. Verify: a new `builtin_tool_names_unchanged_after_split` test compares the tool name list with the baseline list from 0.1.
- [ ] 7.4 `git mv` the services set from design D1 into `crates/agent-services/` (renames only), then the glue commit. Tests that use `TestAgent` or other runtime helpers move to runtime. Verify: package checks for `querymt-agent-services`; the layering check reports zero services violations; the count invariant holds.

## 8. Extract `querymt-agent-runtime` and slim `querymt-agent`

- [ ] 8.1 Add the `ProfileRuntimeHost` trait and `ProfileRuntimeView` in the runtime-to-be. Move `ProfileMetadata`, `SessionProfileBinding`, `ResolvedProfileProviderLock` and `ProfileCatalog` there. Implement the trait for `ProfileRuntimeManager` and change `api::ProfileRuntimeHandle` to `Arc<dyn ProfileRuntimeHost>`. Verify: the `profiles.rs` tests and `agent/handle/tests/*` profile tests pass.
- [ ] 8.2 Move `api/sessions.rs`, `api/mesh/{invite,types}.rs` and `plugin_update.rs` to the runtime-to-be, and re-export them from `api`. Verify: the layering check reports zero runtime→top violations.
- [ ] 8.3 `git mv` the runtime set from design D1 into `crates/agent-runtime/` (renames only). In the glue commit, add the features `remote`, `oauth`, `plugin-loaders`, `api` and `test-support` (exporting `TestAgent`, mocks, `mcp_harness`, drivers). `querymt-agent` forwards its features to them. Move the 11 test files with top-layer references (design D6) into `querymt-agent`. Verify: package checks for `querymt-agent-runtime` with `RUNTIME_FEATURES`; `cargo check -p querymt-agent-runtime --no-default-features`; the count invariant holds.
- [ ] 8.4 Build the `querymt-agent` facade module tree (design D5) so every `querymt_agent::…` path used by examples, benches, `tests/`, `querymt-agent-py` and `querymt-mobile-ffi` resolves. Verify: package checks for `querymt-agent` with `AGENT_FEATURES`; `cargo build -p querymt-agent --example qmtcode --features AGENT_FEATURES`; `cargo bench -p querymt-agent --no-run`.
- [ ] 8.5 Add a log-target test, `agent_log_targets_keep_querymt_agent_prefix`, in `querymt-agent-runtime`. It asserts that `module_path!()` at a known log site starts with `querymt_agent`, and that an `env_logger` filter `querymt_agent=debug` enables it. Update the `docs/docs/agent/mesh.md` filter to `querymt_agent_runtime::agent::remote`. Verify: the test passes, and the doc example is checked by hand against a `RUST_LOG` run.
- [ ] 8.6 Update `generate-types.sh`, CI per-package steps and coverage for runtime and top. Verify: typeshare is byte-identical to the baseline; CI passes.

## 9. Cleanup and measurement

- [ ] 9.1 Remove the temporary internal `crate::x` aliases and rewrite the paths to the real crate paths. Remove the layering check script, its allowance file and its CI step, because Cargo now enforces the graph. Verify: package checks for every agent package; `rg -n "pub\(crate\) use querymt_agent_" crates/agent*` returns nothing.
- [ ] 9.2 Update `AGENTS.md` and `openspec/config.yaml` with the package list, per-layer features, and the rule "test the package you changed, plus `querymt-agent` for facade or downstream changes". Update `CHANGELOG.md` Unreleased with the BREAKING items from the proposal. Verify: the docs build (`mkdocs build` if available); review the diff.
- [ ] 9.3 Repeat the 0.1 timing runs, with edits in a services file, a sqlite file and a runtime file, plus the per-package clippy and nextest runs. Record them below this task, next to the baseline. Verify: the numbers are recorded; this is not a pass/fail gate.

## 10. Downstream and validation

- [ ] 10.1 `cargo check -p querymt-agent-py` (with `PYO3_PYTHON` set to Python 3.10+) and `cargo check -p querymt-mobile-ffi`. Verify: both pass with no source diff in either crate (`git diff --stat crates/py crates/querymt-mobile-ffi` is empty).
- [ ] 10.2 `cargo nextest run -p querymt-agent-py`, if it has Rust tests, and the Python test step from `test-python-packages.yml`. Verify: same pass count as the baseline.
- [ ] 10.3 `openspec validate split-agent-crate --strict`. Verify: exit code 0.

## Workflow follow-up

- Phases 0–1 (tasks 0.1 to 1.8) merge before `feat/kb`. Task 1.9 is done on the `feat/kb` branch after those land. Baselines are retaken after `harden-agent-types-and-errors` lands (task 2.1). Phases 2–9 must follow the gate in group 2.
- Archive the change after every phase has merged and group 10 has passed on `main`.
- Decide whether to split `querymt-agent-runtime` further based on the 9.3 measurements.
