# Tasks

Scenario references use `code-index-cache / <Scenario name>`. Test names are the scenario names in snake_case, so `cargo nextest run -p <package> -E 'test(/<name>/)'` selects them.

## 1. Dependency and Ajika behaviour pins

- [x] 1.1 Add `ajika = "=0.2.1"` and `dirs.workspace = true` to `crates/agent-index/Cargo.toml`. Add a `test-support` feature to `querymt-agent-index`, empty for now; group 4 fills it. Enable that feature only from `querymt-agent-services` `[dev-dependencies]`. Run `cargo hakari generate` and `cargo hakari manage-deps`. Verify with `cargo check -p querymt-agent-index` and `cargo hakari verify`. Then confirm that `cargo tree -i ajika --workspace` lists no provider crate and no `querymt`.
  - Ran `cargo hakari generate` (no changes), `cargo hakari manage-deps` (no operations), `cargo check -p querymt-agent-index` (ok; only unused-dependency warnings until code uses them), `cargo hakari verify` (ok). `cargo tree -i ajika --workspace` lists only agent crates (index, services, runtime, knowledge, agent, py, mobile-ffi): no provider crate, no `querymt`.
- [x] 1.2 Add `crates/agent-index/src/code_cache/ajika_contract_tests.rs`, which pins the Ajika behaviours that design D2, D5 and D6 rely on:
  - (a) opening an already-open directory returns a distinguishable lock error, and the matching `ErrorKind` is recorded in design D5;
  - (b) `forget(SourceFile::TEXT, …)` followed by re-acquiring equal text validates without executing;
  - (c) setting equal text does or does not create a revision (record which in design D6);
  - (d) a reopened database with a different epoch executes again;
  - (e) the exact `#[ajika::input]` field syntax for `text` and `readable`.

  Verify with `cargo nextest run -p querymt-agent-index -E 'test(/ajika_contract/)'`. If (a) or (b) fails, stop and update design.md before group 2.
  - `cargo nextest run -p querymt-agent-index -E 'test(/ajika_contract/)'`: 4 passed, 123 skipped. (a) `InUse`; (b) validates without executing; (c) equal set keeps `executed == 0` and `validated == 0`; (d) new epoch executes again; (e) syntax recorded in design D2. design.md D2, D5 and D6 updated.
- [x] 1.3 Add `RapidHash::from_u64` to `crates/agent-core/src/hash.rs`, with a round-trip unit test next to the existing `RapidHash` tests. Verify with `cargo nextest run -p querymt-agent-core -E 'test(/rapid_hash/)'`.
  - `cargo nextest run -p querymt-agent-core -E 'test(/rapid_hash/)'`: 1 passed, 276 skipped.

## 2. Shared per-file extraction and persisted values (querymt-agent-index)

- [x] 2.1 Fold the per-language bodies of `FunctionIndex::build_sync` (`function_index/core.rs`) into one `index_file_entries(path, source, config) -> Vec<IndexedFunctionEntry>` in `function_index/indexing.rs`. It keeps the oversize skip and the TS/Rust/tree-sitter dispatch, runs per file on the 64 MB `PARSE_POOL` (a `OnceLock<rayon::ThreadPool>` replacing the per-build pool), and is called from `build_sync`. Verify that behaviour is unchanged with the existing suite: `cargo nextest run -p querymt-agent-index -E 'test(/function_index/)'`.
  - `cargo nextest run -p querymt-agent-index -E 'test(/function_index/)'`: 21 passed, 106 skipped.
- [x] 2.2 Add `FunctionIndex::from_entries(root, config, HashMap<PathBuf, Vec<IndexedFunctionEntry>>)` and `FunctionIndex::set_file_entries(path, entries)`. Remove the unused `FunctionIndexConfig::cache_path` and `with_cache_path`. Verify with `cargo check -p querymt-agent-index --all-targets`.
  - `cargo check -p querymt-agent-index --all-targets`: ok (only the unused-dependency warning for `ajika`/`dirs`, resolved once group 3 uses them).
- [x] 2.3 Add `code_cache/values.rs` with private wincode/`PartialEq` mirrors of `SymbolEntry`, `SymbolKind`, `SymbolDigest` and `IndexedFunctionEntry`, which drops `AstFingerprint` (design D4). Add `From` conversions both ways. On load, TypeScript fingerprints are recomputed with `AstFingerprint::from_source`. Add `SymbolsResult` and `FunctionsResult`. Add a round-trip test over the existing multi-language symbol fixtures in `symbol_index/mod.rs` that asserts mirror → public equals the direct `SymbolIndex::from_source` result, and that a TS function's fingerprint similarity to itself is 1.0 after the round-trip. Verify with `cargo nextest run -p querymt-agent-index -E 'test(/code_cache::values/)'`.
  - `cargo nextest run -p querymt-agent-index -E 'test(/code_cache::values/)'`: 5 passed, 127 skipped. Fixtures live in `symbol_index/fixtures.rs` (test-only, six languages). `SymbolEntry` gained `PartialEq, Eq`; `wincode` is a direct dependency pinned to ajika's version.

## 3. Cache owner, queries, storage and stats (querymt-agent-index)

- [x] 3.1 Implement the queries in `code_cache/queries.rs` (design D2/D3):
  - the `SourceFile` and `IndexSettings` inputs, with the `read_source` initializer reading under `Inputs::with_state(WorkspaceRoot)`;
  - `file_symbols`, `file_functions`, and `all_functions` using `db.parallel_map`;
  - leaf parses on `PARSE_POOL`;
  - parse, UTF-8, unreadable and oversize outcomes returned as data;
  - the execution counter in services (design D7).

  Covers `code-index-cache / One broken file among valid files` with test `one_broken_file_among_valid_files`. Verify with `cargo nextest run -p querymt-agent-index -E 'test(/one_broken_file_among_valid_files/)'`.
  - Passed within the combined group 3 run below (7 passed, 132 skipped). `all_functions` is realised as `functions_for` (a plain `parallel_map` over `file_functions`), not a stored query, so one record does not duplicate every file's functions; design D2 updated.
- [x] 3.2 Implement `code_cache/storage.rs`:
  - `CodeIndexStorage`, `CodeIndexCacheOptions` with the spec defaults (256 MiB, 200,000 entries, 30 days), and `CODE_CACHE_SCHEMA_VERSION = 1`;
  - the epoch string;
  - `<base>/querymt/code-index/<{:016x}>` resolution;
  - the open → lock-to-memory / other-error-remove-and-retry-once → memory fallback, with a warning log (design D5).

  Covers:
  - `code-index-cache / Two workspaces have separate caches` (`two_workspaces_have_separate_caches`, which asserts the directory naming and that the same relative path in two roots yields each root's own symbols);
  - `code-index-cache / Cache directory held by another owner` (`cache_directory_held_by_another_owner`);
  - `code-index-cache / Damaged cache is rebuilt` (`damaged_cache_is_rebuilt`);
  - `code-index-cache / Schema version change rebuilds the cache` (`schema_version_change_rebuilds_the_cache`).

  Verify with `cargo nextest run -p querymt-agent-index -E 'test(/two_workspaces_have_separate_caches|cache_directory_held_by_another_owner|damaged_cache_is_rebuilt|schema_version_change_rebuilds_the_cache/)'`.
- [x] 3.3 Implement the owner thread and `CodeCacheClient` in `code_cache/owner.rs` (design D1/D6):
  - commands `IndexAll`, `Apply`, `Symbols` (with the equal-text comparison before setting), `Stats` and `Collect`;
  - text forgotten after each run;
  - `collect` on exit when the last client drops.

  Covers:
  - `code-index-cache / Statistics after a cold build` (`statistics_after_a_cold_build`);
  - `code-index-cache / Idle results are evicted and recomputed` (`idle_results_are_evicted_and_recomputed`, with a millisecond `max_idle`, `Collect`, then a lookup of unchanged text that increments `parsed_files` and returns equal symbols).

  Verify with `cargo nextest run -p querymt-agent-index -E 'test(/statistics_after_a_cold_build|idle_results_are_evicted_and_recomputed/)'`.
  - `cargo nextest run -p querymt-agent-index -E 'test(/one_broken_file_among_valid_files|two_workspaces_have_separate_caches|cache_directory_held_by_another_owner|damaged_cache_is_rebuilt|schema_version_change_rebuilds_the_cache|statistics_after_a_cold_build|idle_results_are_evicted_and_recomputed/)'`: 7 passed, 132 skipped.

## 4. Workspace index integration (querymt-agent-index)

- [x] 4.1 Add `cache: CodeIndexCacheOptions` to `WorkspaceIndexManagerConfig` and pass it into `WorkspaceIndexActor::create`. The actor spawns the owner, builds through `IndexAll` → `FunctionIndex::from_entries`, and stores `code_cache: CodeCacheClient` in `WorkspaceHandle`. Route `Rebuild`, `UpdateFile`, `RemoveFile` and `ApplyChanges` through the owner (one `Apply` per change set) and `set_file_entries`/`remove_file` (design D8). Add `parsed_files` and `reused_files` to `WorkspaceIndexStats`. Tests use `CodeIndexStorage::Directory(tempdir)` and drive change sets through the actor's `ApplyChanges` message, without waiting on notify timing. Covers:
  - `code-index-cache / Restart with unchanged files parses nothing` (`restart_with_unchanged_files_parses_nothing`);
  - `code-index-cache / File edited while the index was closed` (`file_edited_while_the_index_was_closed`);
  - `code-index-cache / Watcher change re-parses only the changed file` (`watcher_change_re_parses_only_the_changed_file`);
  - `code-index-cache / Deleted file leaves duplicate detection` (`deleted_file_leaves_duplicate_detection`).

  Verify with `cargo nextest run -p querymt-agent-index -E 'test(/restart_with_unchanged_files_parses_nothing|file_edited_while_the_index_was_closed|watcher_change_re_parses_only_the_changed_file|deleted_file_leaves_duplicate_detection/)'`.
- [x] 4.2 Under the `test-support` feature, add `CodeCacheClient::unresponsive()` (holds the receiver and never replies) and `CodeCacheClient::disconnected()` (receiver dropped). Add `outline_index::index_symbols(symbols, options)`, which exposes `symbols_to_sections` with `extract_outline`'s error mapping. Verify with `cargo check -p querymt-agent-index --features test-support --all-targets`.
  - `cargo check -p querymt-agent-index --features test-support --all-targets`: ok, no warnings. `unresponsive(root)`/`disconnected(root)` take the workspace root so key mapping still works; `outline_index::index_symbols` has a test pinning it to `index_source` (1 passed).
- [x] 4.3 Run the full package suite and the checks: `cargo t-index`, then `cargo fmt -p querymt-agent-index -- --check`, then `cargo cl-index`. Record the pass/fail/skip counts here.
  - `cargo t-index`: 143 run, 143 passed, 0 failed, 1 skipped (the skip is an existing ignored test, not part of this change). `cargo fmt -p querymt-agent-index -- --check`: clean (after `cargo fmt`). `cargo cl-index`: no warnings or errors.

## 5. Symbol tools read through the index (querymt-agent-services, querymt-agent-runtime)

- [x] 5.1 Add `ToolContext::workspace_index(&self) -> Option<CodeCacheClient>` (default `None`) and `AgentToolContext::with_workspace_index`. Add a shared helper in `tools/builtins/helpers.rs`. It maps an absolute path to a root-relative key, returns `None` outside the root, and calls `Symbols` with a 2 s `CODE_INDEX_LOOKUP_DEADLINE`. On no index, outside root, timeout, send error or reply error, it falls back to `SymbolIndex::from_source_for_path`. Use it in `get_symbol.rs`, `get_function.rs` and `replace_symbol.rs`. `index.rs` reads the file, applies the `max_file_bytes` check, then calls `index_symbols`. `find_references.rs` batches 256 files in walk order and keeps the `max_results` truncation. Tests build a real `CodeCacheClient` over a temp root. Covers:
  - `code-index-cache / Symbol tool output matches with and without the index` (`symbol_tool_output_matches_with_and_without_the_index`, across all five tools and several languages; `replace_symbol` compares the written file);
  - `code-index-cache / Edit not yet reported by the watcher` (`edit_not_yet_reported_by_the_watcher`);
  - `code-index-cache / Repeated lookup on an unchanged file is reused` (`repeated_lookup_on_an_unchanged_file_is_reused`);
  - `code-index-cache / Directory reference search reuses unchanged files` (`directory_reference_search_reuses_unchanged_files`).

  Verify with `cargo nextest run -p querymt-agent-services -E 'test(/symbol_tool_output_matches_with_and_without_the_index|edit_not_yet_reported_by_the_watcher|repeated_lookup_on_an_unchanged_file_is_reused|directory_reference_search_reuses_unchanged_files/)'`.
  - Tests are in `tools/builtins/code_index_tests.rs`. The first run exposed that `find_symbol_references` grouped output with a `HashMap`, so file order varied between runs; `format_matches` now groups in first-seen (walk) order, so file order is deterministic (previously random). The `max_results` cap is also now one total across all search paths and the walk collects one match beyond it, so the "Results capped" note appears whenever more matches exist (previously it could never appear). These are the two visible output changes. Combined 5.1+5.2 run: 8 passed, 1010 skipped.
- [x] 5.2 Add fallback tests. They use `AgentToolContext` without an index, and `CodeCacheClient::unresponsive()` / `disconnected()`. Covers:
  - `code-index-cache / No workspace index yet` (`no_workspace_index_yet`);
  - `code-index-cache / File outside the workspace root` (`file_outside_the_workspace_root`, which asserts output and unchanged stats);
  - `code-index-cache / Index does not answer in time` (`index_does_not_answer_in_time`, which asserts direct output and elapsed time below the deadline plus 3 s);
  - `code-index-cache / Index fails the lookup` (`index_fails_the_lookup`).

  Verify with `cargo nextest run -p querymt-agent-services -E 'test(/no_workspace_index_yet|file_outside_the_workspace_root|index_does_not_answer_in_time|index_fails_the_lookup/)'`. Then run `cargo t-services`, `cargo fmt -p querymt-agent-services -- --check` and `cargo cl-services`.
  - Fallback tests passed in the combined run above (8 passed). `cargo t-services`: 1014 run, 1014 passed, 0 failed, 4 skipped. `cargo fmt -p querymt-agent-services -- --check`: clean. `cargo cl-services`: no warnings.
- [x] 5.3 In `crates/agent-runtime/src/agent/execution_context.rs` `ExecutionContext::tool_context`, set `with_workspace_index` from `runtime.workspace_handle.get()`. Add a runtime test in which the workspace handle is set and a session `get_symbol` call twice on an unchanged file increases `reused_files`. It covers `code-index-cache / Repeated lookup on an unchanged file is reused` end to end (`repeated_lookup_on_an_unchanged_file_is_reused_through_session`). Verify with `cargo t-runtime -E 'test(/repeated_lookup_on_an_unchanged_file_is_reused_through_session/)'`, then `cargo check -p querymt-agent-runtime --features remote,oauth,plugin-loaders,api`, `cargo fmt -p querymt-agent-runtime -- --check` and `cargo cl-runtime`.
- [x] 5.4 Document the cache in `docs/docs/agent/index.md`, near the "Runtime State" and symbol tools text: location, what is stored, the size and idle limits, the memory fallback when another process holds it, and that it is safe to delete. Add a CHANGELOG.md "Unreleased" entry for the persistent code index and the `querymt-agent-index` Rust API changes (marked breaking). Verify that the docs page lists the same path and limits as the spec.

## 6. Integration and measurement

- [x] 6.1 Measure on this repository with a release `qmtcode`:
  - cold and warm workspace index start times;
  - `parsed_files`/`reused_files` after a warm start;
  - the cache directory size;
  - the TS fingerprint recompute time;
  - the release binary size before and after.

  Record the numbers here, and answer design.md's Open Question on `defer_checkpoints`.
  - Method: a throwaway release harness (not committed) drove `WorkspaceIndexActor::create` with `CodeIndexStorage::Directory(tempdir)` on this repository (824 indexed files, 918 candidate source files, 15,273 functions), on an Apple-silicon macOS machine. Release `qmtcode` was built with `cargo build --release -p querymt-agent --example qmtcode`, before from a clean `HEAD` worktree and after from this change.
  - Cold start: 0.42 s, `parsed_files=918`, `reused_files=0`. The same build without the cache (`FunctionIndex::build`) took 0.26 s, so a cold start pays about 0.15 s for writing the cache once.
  - Warm start (3 runs): 0.044-0.046 s, `parsed_files=0`, `reused_files=918`, 15,273 functions, about 9x faster than cold and 6x faster than the uncached build.
  - Cache directory: 5.6 MB for this repository (well under the 256 MiB limit).
  - Tool lookups: 4.3 ms average for changed text (computes and checkpoints), 0.04 ms for unchanged text.
  - TS fingerprint recompute: this repository has no TypeScript, so it was measured on `craft-agents-oss`: 4,509 functions in 0.028 s (3,507 parsed ok; the rest are not stored as entries). Negligible next to a warm start.
  - Release binary size: 168,627,808 bytes before, 171,302,112 bytes after (+2.7 MB, +1.6%).
  - Open Question answered: keep the default eager checkpoints. A lookup that computes a new result costs about 4 ms including the fsync, and unchanged lookups write nothing, so `defer_checkpoints` is not worth its risk of losing results on a crash. design.md updated.
- [x] 6.2 Build downstream packages (from `cargo tree -i querymt-agent-index --workspace`):
  - `cargo check -p querymt-agent-knowledge`;
  - `cargo t-agent --no-run` with `QMT_UI_DIST` pointing at the fixture from `.github/actions/rust-setup/action.yml`;
  - `cargo check -p querymt-agent-py` with `PYO3_PYTHON` set to Python ≥ 3.10;
  - `cargo check -p querymt-mobile-ffi`.

  Note that the zbus/systemd features stay unverified on macOS.
  - `cargo check -p querymt-agent-knowledge`: ok. `QMT_UI_DIST=<minimal fixture outside target/> cargo t-agent --no-run`: built. `PYO3_PYTHON=/opt/homebrew/bin/python3.14 cargo check -p querymt-agent-py`: ok. `cargo check -p querymt-mobile-ffi`: ok. The zbus/systemd features only take effect on Linux, so they remain unverified on macOS.
- [x] 6.3 Run `cargo hakari verify` and `openspec validate persist-code-index-with-ajika --strict`. Both must pass.
  - `cargo hakari verify`: "querymt-workspace-hack works correctly". `openspec validate persist-code-index-with-ajika --strict`: valid. Re-run after the last code change, together with `cargo t-index` (143 passed, 1 skipped), `cargo cl-index` (clean) and `cargo fmt --check` for index, services, runtime and core (clean).

## 7. Review follow-ups

- [x] 7.1 Share on-disk cache owners per directory through a process-wide registry, and let a new owner wait for the previous one to release the directory; make the workspace actor's change forwarder hold a `WeakActorRef` (design D1). Tests: `recreated_index_shares_the_live_owner`, `dropping_every_client_releases_the_directory_for_the_next_owner`; `cache_directory_held_by_another_owner` now holds the directory with a separate database, as another process would.
- [x] 7.2 Sweep stale sibling cache directories (unlocked, unused for `max_idle`), touch the lock file on open and close, create the cache base with mode `0700` (design D9). Tests: `stale_directories_are_swept` (also pins Ajika's `owner.lock` name), `cache_base_is_private`.
- [x] 7.3 One `SymbolResolver` per tool call that stops asking the index after the first timeout or error; `find_symbol_references` parses unanswered files lazily, applies `max_results` to the total across paths, and reports capped results (design D6). Tests: `busy_index_costs_one_deadline_per_call`, `reference_search_limit_applies_across_paths`.
- [x] 7.4 Index files whose path cannot be keyed directly instead of failing the whole cache operation (design D2). Test: `non_utf8_file_name_does_not_disable_the_cache` (Linux only; macOS file systems reject such names).
- [x] 7.5 Fold an extractor/parser fingerprint from `build.rs` into the epoch (design D5).
- [x] 7.6 Remove the equal-text comparison that could never match; one `text: Option<String>` input so each file is read once; count reuse only for successful operations; keep `SymbolError::Io` distinct; lock-free statistics; survive panics in an operation (design D1, D2, D6, D7). Test: `parser_panic_does_not_stop_the_owner`.
- [x] 7.7 Delete the cache directory only for `InvalidCache`; other open errors fall back to memory (design D5).
- [x] 7.8 Reuse the entries (with TypeScript fingerprints) computed in an operation instead of recomputing fingerprints (design D8).
- [x] 7.9 Drop `IndexError::CodeCache` (cache errors never surface); `CodeCacheError::Operation` keeps the Ajika error as its source; add `CodeCacheError::ParsePool`; the test-only receiver no longer changes `CodeCacheClient`'s shape.
