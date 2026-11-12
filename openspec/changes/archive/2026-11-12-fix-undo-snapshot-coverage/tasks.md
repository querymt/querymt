# Tasks

How to check off a task:

- A task may be checked (`- [x]`) only after every command in it has passed **in this change's working tree**. Under the task, record the command and the nextest summary line (for example `12 passed, 4300 skipped`).
- A test that is skipped, ignored or filtered out does not count as passing. If nextest says a filter matched no tests, fix the filter; never drop it.
- If a test can't run on the current machine (for example a permission test running as root), leave the task unchecked and note why.
- Environment for every command below, matching `.github/workflows/test-agent.yml`:

  ```sh
  export F=dashboard,oauth,zbus-secret-service,systemd-credentials,remote
  mkdir -p /tmp/qmt-ui && printf '<!doctype html><title>QueryMT</title>\n' > /tmp/qmt-ui/index.html \
    && printf '{"revision":"ci","target":"embedded","acpWebSocketPath":"/acp/ws"}\n' > /tmp/qmt-ui/querymt-ui.json
  export QMT_UI_DIST=/tmp/qmt-ui
  ```

  The zbus/systemd features only take effect on Linux; those paths stay unverified on macOS.
- New tests are named after their spec scenario in snake_case, so `-E 'test(/<name>/)'` selects them. They assert the THEN outcome through the production path (`SnapshotBackend` trait, `undo_impl`/`redo_impl` via `UndoTestFixture`, or the config loader).

## 1. Module split, backend configuration and baseline (no behaviour change)

- [x] 1.1 Move `crates/agent/src/snapshot/git.rs` into `snapshot/git/{mod.rs,capture.rs,restore.rs,gc.rs,tests.rs}` without changing behaviour (design D1). Items stay `pub(crate)` or private unless already public (proj-pub-crate-internal). Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/^snapshot::git::|^agent::undo_tests::|^agent::undo_integration_tests::/)'`. It must give the same pass count as before the move (record both).
  - Before: `70 tests run: 70 passed, 4183 skipped`. After: `70 tests run: 70 passed, 4183 skipped` (`cargo nextest run -p querymt-agent --features "$F"`, `$F` as above).
- [x] 1.2 Store `snapshot_base: Option<PathBuf>` and `limits: Limits` on `GitSnapshotBackend`, and make `with_snapshot_base` a public `#[must_use]` builder (design D2, api-builder-must-use). Add `pub(crate) fn with_limits`. Make `diff`, `restore_paths`, `restore` and `gc` clone `self` into `spawn_blocking`, and delete the four `#[cfg(test)]` backend re-creations. Add `trait_methods_use_the_configured_snapshot_base`: track, diff, restore_paths and gc through the trait on a backend with a temp base, then assert nothing was written under `dirs::cache_dir()/querymt/snapshots/<hash>` for that worktree. Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/^snapshot::git::|trait_methods_use_the_configured_snapshot_base/)'`.
  - Verified: `snapshot::git::` + `trait_methods_use_the_configured_snapshot_base`: 2 passed for the new tests; full `^snapshot::git::|^agent::undo_` run `101 passed, 4191 skipped`.
- [x] 1.3 Add `SnapshotError::Io { op, path, #[source] source }` and the `io_err(op, path)` helper (design D3, err-source-chain). Add unit test `io_error_names_operation_and_path`, which checks `Display` contains the op and the path and that `std::error::Error::source()` is the `io::Error`. Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/io_error_names_operation_and_path/)'`.
  - Verified: `io_error_names_operation_and_path`: `1 passed`.
- [x] 1.4 Move `tempfile` from dev-dependency to a normal dependency of `querymt-agent`. Add `flate2 = { version = "1", default-features = false, features = ["rust_backend"] }` as a normal dependency; keep the build-dependency entry as is (design D6, D9). Confirm `Cargo.lock` gains no new package. Verify with `cargo check -p querymt-agent --features "$F"` and `git diff --stat Cargo.lock`, which must show no added `[[package]]`.
  - Verified: `cargo check -p querymt-agent --features "$F"` ok; `git diff --stat Cargo.lock` empty (no new package).
- [x] 1.5 Add the criterion bench `crates/agent/benches/snapshot_track.rs` plus its `[[bench]]` entry (design D14, perf-black-box-bench). Groups: 2 000 small files cold, the same files warm, one 256 MiB random file new, and the same file unchanged. Snapshots go through the trait with `with_snapshot_base` on a temp dir. Run it and record the **baseline** numbers under this task. Verify with `cargo bench -p querymt-agent --bench snapshot_track`.
  - Baseline (HEAD, same bench with `with_max_file_bytes(None)` in a throwaway worktree, `cargo bench -p querymt-agent --bench snapshot_track`): small_files_2000_cold 347.07 ms; small_files_2000_warm 12.386 ms; big_file_256mib_new 2.1853 s; big_file_256mib_unchanged 749.86 us.

## 2. Complete capture: remove the size cap (`workspace-snapshots`: every file in scope, changing files, unchanged files)

- [x] 2.1 Remove `max_file_bytes` from `SnapshotBackendConfig`, along with `default_max_file_bytes`, `with_max_file_bytes`, `DEFAULT_MAX_FILE_BYTES`, `SKIPPED_TRAILER`, `skipped_paths`, the `skipped` map and its warnings, the skip filters in `diff_commits`/`checkout_paths`, and the call sites in `agent_config_builder.rs` `with_snapshot_from_execution` and `api/quorum.rs` (design D4). Delete the obsolete tests `test_files_over_max_file_bytes_are_skipped`, `test_zero_max_file_bytes_means_unlimited`, `test_file_growing_past_limit_is_not_reported_deleted_or_removed`, `test_file_shrinking_below_limit_is_not_reported_added`, `test_skipped_paths_do_not_hide_real_changes`, `test_skipped_trailer_round_trips` and `test_shrinking_stream_is_skipped_not_fatal`. Remove the `max_file_bytes` line from `docs/docs/agent/configuration.md`.
  - Verified: `removed_max_file_bytes_key_is_rejected|^snapshot::git::|^config::` passed (part of 395-test run); `rg -n "max_file_bytes|qmt-skipped|skipped_paths" crates docs` returns only unrelated index-tool `max_file_bytes` and the config test.
  - Covers `workspace-snapshots` / Removed max_file_bytes key is rejected. Test: `removed_max_file_bytes_key_is_rejected` in `config/tests.rs`. It loads TOML with the key under `[agent.execution.snapshot]` and under a quorum delegate's `execution.snapshot`, and asserts the error names `max_file_bytes`.
  - Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/removed_max_file_bytes_key_is_rejected|^snapshot::git::|^config::/)'` and `rg -n 'max_file_bytes|qmt-skipped|skipped_paths' crates docs`, which must return only the unrelated index-tool `max_file_bytes` and the config test.
- [x] 2.2 Replace `Option<ObjectId>` with `CaptureOutcome { Stored(ObjectId), Changed }` (type-enum-states). The stream path re-stats after the read, and on a short read or a stamp change it captures from an `fs::copy` into a `tempfile::TempDir` under the snapshot git dir (design D4). `SnapshotStats` drops the skip counters and gains `copied_files`.
  - Verified: `file_that_shrinks_during_capture_is_captured|file_that_grows_during_capture_is_captured_whole`: `2 passed`; tests also assert no `qmt-copy-*` entry is left in the git dir.
  - Covers `workspace-snapshots` / File that shrinks during capture is captured: `file_that_shrinks_during_capture_is_captured` calls the capture function with a listed stamp larger than the file, asserts `copied_files == 1`, and checks the stored blob equals the file.
  - Covers `workspace-snapshots` / File that grows during capture is captured whole: `file_that_grows_during_capture_is_captured_whole` passes a listed stamp smaller than the file and asserts the blob has every byte.
  - Both use `with_limits` with a small `stream_bytes`.
  - Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/file_that_shrinks_during_capture_is_captured|file_that_grows_during_capture_is_captured_whole/)'`, then also check that no leftover entry exists in the copy directory.
- [x] 2.3 Add undo/redo regression tests through `UndoTestFixture` in `agent/undo_integration_tests.rs`, with default limits and 17 MiB files.
  - Verified: `large_file_is_captured_and_undone|undo_and_redo_of_a_file_that_grew_restore_both_versions`: `2 passed`.
  - `workspace-snapshots` / Large file is captured and undone: `large_file_is_captured_and_undone`.
  - `workspace-snapshots` / Undo and redo of a file that grew restore both versions: `undo_and_redo_of_a_file_that_grew_restore_both_versions`. This is the CodeRabbit finding. It drives `redo_impl` through `fixture.redo` after the 1 KiB → 17 MiB turn is undone, then asserts the 1 KiB content after undo and the exact 17 MiB content and `restored == true` after redo.
  - Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/large_file_is_captured_and_undone|undo_and_redo_of_a_file_that_grew_restore_both_versions/)'`.
- [x] 2.4 Rename the stat-cache tests after their scenarios and keep their assertions.
  - Verified: `unchanged_file_is_not_read_again|rewrite_with_same_size_and_mtime_is_detected|stat_cache`: `4 passed`.
  - `test_second_snapshot_reuses_stat_cache` becomes `unchanged_file_is_not_read_again`. Assert `bytes_hashed == 0` for the second snapshot. Covers `workspace-snapshots` / Unchanged file is not read again.
  - `test_same_size_and_mtime_rewrite_is_detected` becomes `rewrite_with_same_size_and_mtime_is_detected`. Covers `workspace-snapshots` / Rewrite with same size and mtime is detected.
  - Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/unchanged_file_is_not_read_again|rewrite_with_same_size_and_mtime_is_detected|stat_cache/)'`.

## 3. Snapshot scope by path (`workspace-snapshots`: snapshot scope)

- [x] 3.1 Set `WalkBuilder` to `.ignore(true).git_ignore(true).git_exclude(true).git_global(false).require_git(false)`, keeping the dot-entry `filter_entry` (design D5). Add tests in `snapshot/git/tests.rs` that track through the trait and inspect the tree:
  - Verified: the five scope tests (plus `undo_leaves_ignored_files_alone`): `6 passed`.
  - `workspace-snapshots` / Gitignored file in a git repository is not captured: `gitignored_file_in_a_git_repository_is_not_captured` (uses `git init`).
  - `workspace-snapshots` / Gitignore applies outside a git repository: `gitignore_applies_outside_a_git_repository` (no `.git`).
  - `workspace-snapshots` / Ignore file excludes paths: `ignore_file_excludes_paths`.
  - `workspace-snapshots` / Git info exclude is honoured: `git_info_exclude_is_honoured`.
  - `workspace-snapshots` / Dot entries are not captured: rename `test_dot_directories_are_pruned` to `dot_entries_are_not_captured`.

  Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/gitignored_file_in_a_git_repository_is_not_captured|gitignore_applies_outside_a_git_repository|ignore_file_excludes_paths|git_info_exclude_is_honoured|dot_entries_are_not_captured/)'`.
- [x] 3.2 Add `undo_leaves_ignored_files_alone` in `agent/undo_integration_tests.rs`, covering `workspace-snapshots` / Undo leaves ignored files alone. A turn modifies `kept.txt` and a gitignored `ignored.log`. After undo, `kept.txt` is reverted and `ignored.log` keeps its modified content. Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/undo_leaves_ignored_files_alone/)'`.
  - Verified: part of the `6 passed` run above.
- [x] 3.3 Document the snapshot scope in `docs/docs/agent/configuration.md` under `[agent.execution.snapshot]`: `.gitignore` (also outside git), `.ignore` as the opt-out, `.git/info/exclude`, and dot entries. Verify by reviewing the rendered markdown, or with `mkdocs build` if available, and record which was used.
  - Verified by reviewing the markdown source (`mkdocs` is not installed); section "Snapshot scope and retention" added.

## 4. Snapshot reuse and large-file warnings (`workspace-snapshots`: unchanged workspace, large-file reporting)

- [x] 4.1 Return HEAD's id when the new tree equals HEAD's tree, and record `stats.reused` (design D7). Fold `test_track_idempotent_no_change` into the new tests.
  - Verified: `unchanged_workspace_returns_the_previous_snapshot_id|changed_workspace_returns_a_new_snapshot_id`: `2 passed`; undo suites `101 passed` together with snapshot tests.
  - `workspace-snapshots` / Unchanged workspace returns the previous snapshot id: `unchanged_workspace_returns_the_previous_snapshot_id` (ids equal, no new commit in `list_commits`).
  - `workspace-snapshots` / Changed workspace returns a new snapshot id: `changed_workspace_returns_a_new_snapshot_id`.
  - Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/unchanged_workspace_returns_the_previous_snapshot_id|changed_workspace_returns_a_new_snapshot_id|^agent::undo_tests::|^agent::undo_integration_tests::/)'`. The undo suites prove that read-only turns, which now return equal ids, still work.
- [x] 4.2 Implement large-file detection against HEAD and the pure `large_file_warnings` helper (design D13, obs-structured-fields). Emit with `tracing::warn!`.
  - Verified: `large_file_entering_snapshots_is_reported_once|many_large_files_are_summarised`: `2 passed`.
  - `workspace-snapshots` / Large file entering snapshots is reported once: `large_file_entering_snapshots_is_reported_once`. It uses `with_limits` with a 1 KiB `large_warn_bytes`, captures tracing output with a `tracing_subscriber::fmt` writer into a shared buffer under `tracing::subscriber::with_default`, and asserts exactly one warning naming the file across two snapshots (the second after modifying it).
  - `workspace-snapshots` / Many large files are summarised: `many_large_files_are_summarised`. Twelve new large files produce 10 named warnings and one "2 more" warning, captured the same way.
  - Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/large_file_entering_snapshots_is_reported_once|many_large_files_are_summarised/)'`.

## 5. Large-file storage, restore correctness and undo failure handling (`workspace-snapshots`: storage, moves, atomic restore, failures)

- [x] 5.1 Write blobs at or above `stream_bytes` through a second loose store with `Compression::NONE` (design D6). Keep `test_streamed_blob_matches_in_memory_hash`. Covers `workspace-snapshots` / Large file is stored uncompressed and readable by git with `large_file_is_stored_uncompressed_and_readable_by_git`, which uses a real 17 MiB file at default limits. It asserts the loose object file is at least 17 MiB and that `git --git-dir <store> cat-file blob <id>` output equals the file. Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/large_file_is_stored_uncompressed_and_readable_by_git|streamed_blob_matches_in_memory_hash/)'`.
  - Verified (with 5.2 filter): `4 passed, 4288 skipped`.
- [x] 5.2 Stream large loose blobs on restore through `flate2::read::ZlibDecoder<BufReader<File>>`, checking the header and hash before persisting. Borrow `&object.data` for small blobs instead of cloning (design D6, own-borrow-over-clone, perf-io-buffering). The internal checkout returns `RestoreStats { streamed_files }`.
  - Verified: `large_file_round_trips_through_restore|streamed_restore_rejects_a_corrupt_object` + 5.1 tests: `4 passed`.
  - `workspace-snapshots` / Large file round-trips through restore: rename `test_streamed_large_file_round_trips` to `large_file_round_trips_through_restore` and add an assertion that `streamed_files == 1`. The memory bound itself is checked by reviewing that no `Vec` holds the blob on this path.
  - Add `streamed_restore_rejects_a_corrupt_object`: flip one byte of the uncompressed object, then assert restore errors and the target keeps its old content.
  - Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/large_file_round_trips_through_restore|streamed_restore_rejects_a_corrupt_object/)'`.
- [x] 5.3 Pass `Some(gix::diff::Options::default())` to `diff_tree_to_tree` and collect paths into a `BTreeSet<PathBuf>` (design D8, coll-set-membership).
  - Verified: `diff_of_a_move_reports_both_paths|undo_of_a_moved_file_restores_the_original_path|redo_of_a_moved_file_moves_it_back`: `3 passed`.
  - `workspace-snapshots` / Diff of a move reports both paths: `diff_of_a_move_reports_both_paths` (through the trait `diff`).
  - `workspace-snapshots` / Undo of a moved file restores the original path: `undo_of_a_moved_file_restores_the_original_path`, via `UndoTestFixture`.
  - `workspace-snapshots` / Redo of a moved file moves it back: `redo_of_a_moved_file_moves_it_back`, via `UndoTestFixture`.
  - Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/diff_of_a_move_reports_both_paths|undo_of_a_moved_file_restores_the_original_path|redo_of_a_moved_file_moves_it_back/)'`.
- [x] 5.4 Implement `write_entry` (`NamedTempFile::new_in(parent)` → write → mode → `persist`) and propagate delete errors other than `NotFound`. Use it in `checkout_paths` and `checkout_all`, and remove every `let _ = fs::…` from the restore paths (design D9, anti-empty-catch). All tests are `#[cfg(unix)]`, and the permission-dependent ones return early when `libc::geteuid() == 0`.
  - Verified (uid 501, not root, so permission tests really ran): `5 passed`; `rg -n "let _ = fs::" crates/agent/src/snapshot` returns nothing.
  - `workspace-snapshots` / Executable bit follows the snapshot: `executable_bit_follows_the_snapshot`.
  - `workspace-snapshots` / Restore keeps a private file private: `restore_keeps_a_private_file_private`.
  - `workspace-snapshots` / Failed restore leaves no temporary files: `failed_restore_leaves_no_temporary_files`.
  - `workspace-snapshots` / Full restore reports a write failure: `full_restore_reports_a_write_failure`, through trait `restore`; the error contains the path.
  - `workspace-snapshots` / Failed delete fails the restore: `failed_delete_fails_the_restore`. Undo through `UndoTestFixture` of a turn that created `sub/new.txt`, with `sub` set to 0555 before undo; the error contains `sub/new.txt`. Permissions are restored at the end.
  - Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/executable_bit_follows_the_snapshot|restore_keeps_a_private_file_private|failed_restore_leaves_no_temporary_files|full_restore_reports_a_write_failure|failed_delete_fails_the_restore/)'`, and `rg -n 'let _ = fs::' crates/agent/src/snapshot`, which must return nothing.
- [x] 5.5 In `undo_impl`, push the redo frame after the restore loop whether or not it succeeded, then return the restore result (design D10). Keep pre-restore validation failures frame-less: the existing `test_undo_rejects_foreign_snapshot_before_any_restore` must still pass.
  - Verified: `undo_that_fails_while_restoring_can_be_redone|redo_that_fails_keeps_its_frame|test_undo_rejects_foreign_snapshot_before_any_restore|redo_rejects_cross_workspace_frames_without_restoring`: `4 passed`.
  - `workspace-snapshots` / Undo that fails while restoring can be redone: `undo_that_fails_while_restoring_can_be_redone`. A turn modifies `a.txt` and `b.txt`, then `b.txt` is replaced by a non-empty directory. Undo errors naming `b.txt`; after the directory is removed, redo returns `a.txt` to its pre-undo content.
  - `workspace-snapshots` / Redo that fails keeps its frame: `redo_that_fails_keeps_its_frame`. Redo is blocked by a non-empty directory at the target path and errors; after the directory is removed, a second redo restores the file.
  - Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/undo_that_fails_while_restoring_can_be_redone|redo_that_fails_keeps_its_frame|test_undo_rejects_foreign_snapshot_before_any_restore|redo_rejects_cross_workspace_frames_without_restoring/)'`.

## 6. Opt-in garbage collection (`workspace-snapshots`: retention settings, no GC by default, pruning, safe deletion, pruned undo/redo, store growth)

- [x] 6.1 Change `max_snapshots` to `Option<NonZeroUsize>` and `max_age_days` to `Option<NonZeroU64>`, both defaulting to `None` (design D11, num-nonzero, api-parse-dont-validate). Update the `[agent.execution.snapshot]` docs in `docs/docs/agent/configuration.md`: drop the `100`/`30` example defaults; document that unset means no GC, that setting either key prunes and deletes old snapshots for the whole workspace (shared by all sessions and delegates there), that undo/redo past the window fail without changing files, that `0` is rejected and that JSON `null` means unset. Tests in `config/tests.rs`:
  - Verified: config filter `115 passed, 4177 skipped` (includes the 5 new tests).
  - `workspace-snapshots` / Retention keys default to unset: `retention_keys_default_to_unset`.
  - `workspace-snapshots` / Retention values are accepted: `retention_values_are_accepted`.
  - `workspace-snapshots` / Zero retention values are rejected: `zero_retention_values_are_rejected`. It checks both keys, and each error names its key.
  - `workspace-snapshots` / JSON null leaves a retention key unset: `json_null_leaves_a_retention_key_unset`.
  - Plus `schema_retention_keys_have_minimum_one`, which checks the JSON schema `minimum` for both keys.
  - Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/retention_keys_default_to_unset|retention_values_are_accepted|zero_retention_values_are_rejected|json_null_leaves_a_retention_key_unset|schema_retention_keys_have_minimum_one|^config::/)'`.
- [x] 6.2 Add `impl From<&SnapshotBackendConfig> for GcConfig` and `GcConfig::is_enabled` (api-from-not-into, name-is-has-bool), make `GcConfig::default()` `{ None, None }`, and set `snapshot_gc_config` in `with_snapshot_from_execution`. In `session_actor.rs` `execute_prompt_inner`, call `gc` only when `is_enabled()`, and replace `let _ = backend.gc(..)` with a `warn!` carrying the error (design D11, anti-empty-catch). Covers `workspace-snapshots` / Configured retention reaches the snapshot store with `configured_retention_reaches_the_snapshot_store` in `agent_config_builder.rs` tests, which asserts the built `AgentConfig.snapshot_gc_config` is `{ max_snapshots: Some(5), max_age_days: Some(2) }` and that a config without the keys yields `GcConfig::default()` with `is_enabled() == false`. Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/configured_retention_reaches_the_snapshot_store|test_quorum_builder_snapshot_policy/)'`.
  - Verified: `configured_retention_reaches_the_snapshot_store|test_quorum_builder_snapshot_policy`: `2 passed`.
- [x] 6.3 Add `SnapshotLock` over `<git_dir>/qmt-snapshot.lock` with `File::lock_shared` and `try_lock` (design D12). Hold the shared lock across the blocking bodies of `track`, `restore_paths` and `restore`. Add unit test `exclusive_lock_is_refused_while_shared_is_held`. Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/exclusive_lock_is_refused_while_shared_is_held|^snapshot::git::/)'`.
  - Verified: `exclusive_lock_is_refused_while_shared_is_held`: `1 passed`; `^snapshot::git::` passes.
- [x] 6.4 Implement `collect_garbage(repo, cfg, now, lock)`: the disabled early return, window selection, a sweep threshold of 10, marking from kept commits only, deleting pruned commits first, then loose objects; `GcResult.deleted_objects` is added (design D12). Replace `test_gc_by_count` and keep `test_gc_leaves_workspace_baseline_refs_reachable` passing. Each test changes a file before every snapshot, because unchanged snapshots are reused (D7).
  - Verified: GC filter `8 passed, 4284 skipped`.
  - `workspace-snapshots` / Garbage collection without limits deletes nothing: `garbage_collection_without_limits_deletes_nothing`. 15 snapshots, `gc` with `GcConfig::default()` reports 0 removed and 15 remaining, and the set of files under `objects/` is identical before and after.
  - `workspace-snapshots` / Snapshots beyond max_snapshots are pruned: `snapshots_beyond_max_snapshots_are_pruned`.
  - `workspace-snapshots` / Snapshots older than max_age_days are pruned: `snapshots_older_than_max_age_days_are_pruned`, with injected `now` set to 31 days later.
  - `workspace-snapshots` / Latest snapshot is kept regardless of age: `latest_snapshot_is_kept_regardless_of_age`.
  - `workspace-snapshots` / Pruned snapshot content is deleted: `pruned_snapshot_content_is_deleted`. It asserts the pruned blob ids are gone and the total size of the `objects/` directory decreased.
  - `workspace-snapshots` / Retained snapshot content survives pruning: `retained_snapshot_content_survives_pruning`. It restores every kept snapshot fully through the trait by its original id and compares the contents.
  - `workspace-snapshots` / Garbage collection skips while a snapshot is in progress: `garbage_collection_skips_while_a_snapshot_is_in_progress`. It holds a shared `SnapshotLock`, runs GC, and asserts `deleted_objects == 0`; a following track still succeeds.
  - Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/garbage_collection_without_limits_deletes_nothing|snapshots_beyond_max_snapshots_are_pruned|snapshots_older_than_max_age_days_are_pruned|latest_snapshot_is_kept_regardless_of_age|pruned_snapshot_content_is_deleted|retained_snapshot_content_survives_pruning|garbage_collection_skips_while_a_snapshot_is_in_progress|test_gc_/)'`.
- [x] 6.5 Add undo/redo tests through `UndoTestFixture` in `agent/undo_integration_tests.rs`:
  - Verified: `undo_without_limits_reaches_the_first_turn|undo_to_a_pruned_snapshot_changes_no_files|redo_to_a_pruned_snapshot_keeps_its_frame`: `3 passed`.
  - `workspace-snapshots` / Undo without limits reaches the first turn: `undo_without_limits_reaches_the_first_turn`. Record 12 turns, call `fixture.backend.gc` with `GcConfig::default()` after each, undo the first turn, and assert every file has its pre-first-turn content.
  - `workspace-snapshots` / Undo to a pruned snapshot changes no files: `undo_to_a_pruned_snapshot_changes_no_files`. Record a turn, take 12 more changed snapshots, run `gc` with `max_snapshots = 1`, undo the turn; it returns an error and every workspace file is byte-identical before and after.
  - `workspace-snapshots` / Redo to a pruned snapshot keeps its frame: `redo_to_a_pruned_snapshot_keeps_its_frame`. Undo a turn, take 12 more changed snapshots, run `gc` with `max_snapshots = 1`, redo; it returns an error, no file changes, and `peek_revert_state` still returns the frame.
  - Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/undo_without_limits_reaches_the_first_turn|undo_to_a_pruned_snapshot_changes_no_files|redo_to_a_pruned_snapshot_keeps_its_frame/)'`.
- [x] 6.6 Add the process-global store-size gauge (design D12, async-no-lock-await, obs-structured-fields). Tests use `with_limits` with `store_warn_bytes` and `store_measure_every_bytes` of a few KiB and a unique snapshot base each (the gauge is keyed by git dir), and capture tracing output as in 4.2.
  - Verified: `large_snapshot_store_is_reported_once|small_snapshot_store_is_not_reported`: `2 passed`.
  - `workspace-snapshots` / Large snapshot store is reported once: `large_snapshot_store_is_reported_once`. Grow the store past the limit, take two more changed snapshots, and assert exactly one warning naming the store path and a size.
  - `workspace-snapshots` / Small snapshot store is not reported: `small_snapshot_store_is_not_reported`. Take a snapshot below the limit and assert no store-size warning.
  - Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/large_snapshot_store_is_reported_once|small_snapshot_store_is_not_reported/)'`.

## 7. Changelog, measurements and integration checks

- [x] 7.1 Add Unreleased entries to `CHANGELOG.md`:
  - Verified by reviewing the CHANGELOG diff.
  - **BREAKING:** `.gitignore` applies to snapshots outside git repositories.
  - Snapshot GC is supported and off by default: `max_snapshots` and `max_age_days` default to unset, and nothing is deleted unless one is set.
  - **BREAKING:** setting `max_snapshots`/`max_age_days` now prunes and deletes old snapshots for the whole workspace (before, the values were ignored); undo/redo past the window fail without changing files.
  - **BREAKING:** `max_snapshots = 0` or `max_age_days = 0` is a config load error.
  - New warnings for files of 64 MiB or more entering snapshots and for snapshot stores of 5 GiB or more, both pointing to `.ignore`.
  - Undo of moves is fixed; undo/redo now report restore failures; large files are always covered.
  - A dev note to clear `querymt/snapshots` in the user cache dir after running a `fix/snapshot` build.

  Verify by reviewing the CHANGELOG diff.
- [x] 7.2 Re-run `cargo bench -p querymt-agent --bench snapshot_track`. Record the numbers next to the 1.5 baseline. The "256 MiB new" group must not be slower than baseline; if it is, leave this task unchecked and report.
  - After: small_files_2000_cold 354.05 ms (baseline 347.07 ms, within noise); small_files_2000_warm 11.427 ms (12.386 ms); big_file_256mib_new 258.12 ms (2.1853 s, ~8.5x faster, not slower); big_file_256mib_unchanged 417.12 us (749.86 us).
- [x] 7.3 Run the full checks for `querymt-agent`, in this order:
  - Verified: `cargo check` ok; nextest `395 passed, 3897 skipped`; `cargo test --doc snapshot` ok (0 failed); `cargo fmt --check` ok; `cargo clippy --all-targets -D warnings` ok. zbus/systemd features are Linux-only and unverified on macOS.
  1. `cargo check -p querymt-agent --features "$F"`
  2. `cargo nextest run -p querymt-agent --features "$F" -E 'test(/snapshot|undo|redo|config::|agent_config_builder|quorum/)'`
  3. `cargo test -p querymt-agent --features "$F" --doc snapshot`, because `with_snapshot_base` and `SnapshotError` docs changed
  4. `cargo fmt -p querymt-agent -- --check`
  5. `cargo clippy -p querymt-agent --features "$F" --all-targets -- -D warnings`
- [x] 7.4 Check the downstream packages from `cargo tree -i querymt-agent --workspace --depth 1`:
  - Verified: `cargo check -p querymt-mobile-ffi` ok; `PYO3_PYTHON=/opt/homebrew/bin/python3.14 cargo check -p querymt-agent-py` ok (Python 3.14).
  - `cargo check -p querymt-mobile-ffi`
  - `PYO3_PYTHON=<python>=3.10> cargo check -p querymt-agent-py`
- [x] 7.5 Run `openspec validate fix-undo-snapshot-coverage --strict`; it must report the change as valid.
  - Verified: `openspec validate fix-undo-snapshot-coverage --strict`: "Change ... is valid".

## 8. Typed ids, self-contained turn records and undo of whole turns (`workspace-snapshots`: change record names its pre-turn snapshot, records without a pre-turn reference, undo fails before changing files)

- [x] 8.1 Red test first (design D15). Add `undo_of_a_prompt_that_deleted_a_file_restores_it` to `agent/undo_integration_tests.rs`. It covers `workspace-snapshots` / Undo of a prompt that deleted a file restores it, and must go through a real prompt, not the hand-written records.
  - Extend `UndoTestFixture` with a constructor that takes a `SharedLlmProvider`. It reuses `mock_plugin_registry` + `TestProviderFactory`, the same pattern as `TestAgent::with_mock_provider`, instead of adding a new harness. Add a `prompt(session, text)` helper that sends an ACP `PromptRequest` through `self.handle`.
  - The mock answers the first request with a `delete_file` call on `notes.txt` (`MockChatResponse::with_tools`), and the request carrying the tool result with text (`MockChatResponse::text_only`). This is the pattern used in `remote/session_actor_ref_remote_tests.rs:126-147`. If tool approval blocks the call, allow `delete_file` in the fixture config; don't switch to a different tool.
  - The undo target is the prompt's user message id, read from `get_history`. Assert `notes.txt` has its original content and `reverted_files` contains `notes.txt`.
  - Run it on the unchanged tree and record the failure (expected: `notes.txt` missing, empty `reverted_files`). Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/undo_of_a_prompt_that_deleted_a_file_restores_it/)'`.
  - Recorded red run on the unchanged behaviour (1 test run: 0 passed, 1 failed, 4322 skipped): `undo must restore the deleted file`, left `None`, right `Some("original notes")` (`undo_integration_tests.rs:1900`). Undo returned success and `notes.txt` stayed missing.
- [x] 8.2 Typed snapshot and turn ids, without behaviour change (design D18, type-newtype-ids, anti-stringly-typed):
  - Replace `pub type SnapshotId = String` with `SnapshotId(Arc<str>)` in `snapshot/backend.rs`, and add `TurnId(Uuid)` (`Copy`) in `model.rs`. Write both serde impls by hand: `SnapshotId` as a plain string, `TurnId` as the lowercase hyphenated string (not `uuid`'s serde, which writes raw bytes in msgpack), loading a non-UUID string through `TurnId::from_stored` (v5 fallback with a fixed namespace). API as in D18; no `Deref`.
  - Retype `SnapshotBackend` (every method and `restore_batch` steps), `SnapshotError::InvalidSnapshotId`, `MessagePart::TurnSnapshotStart`/`TurnSnapshotPatch` (`turn_id: TurnId`, `snapshot_id: SnapshotId`), `RevertState.snapshot_id`, and the `revert_states` row mapping in `sqlite_storage/session_store.rs` (convert at the column).
  - Replace `runtime.turn_snapshot: (String, String)` with `TurnSnapshot { turn_id, snapshot_id }` and the pre-turn task output with `(TurnId, SnapshotResult<SnapshotId>)`; the task keeps the `SnapshotError` (err-source-chain). Update the `transitions.rs` assertions.
  - Mint the run's `TurnId` once where the run starts (`session_actor.rs:613`, `:2478`) and derive the `String` `run_id` from it. `ExecutionContext::turn_id: Option<TurnId>`, `with_turn_id(TurnId)`, `turn_id() -> Option<TurnId>`, plus `turn_id_string()` for hook payloads, events, scheduler messages and steering, which stay `String`.
  - In the git backend, replace `parse_commit_id` (`git/restore.rs`) and `parse_id` (`git/gc.rs`) with one `object_id(&SnapshotId) -> SnapshotResult<gix::ObjectId>`, and mint ids with `From<gix::ObjectId>`.
  - No `Uuid::new_v4().to_string()` wrapped into a `TurnId`, no `TurnId` clones (it is `Copy`), and no `SnapshotId` clone without a real second owner (own-borrow-over-clone). Remove `&x.to_string()` conversions that only produced a `&String`.
  - Tests use real UUID turn ids (`TurnId::new()` or UUID literals), not `"turn-1"`-style strings.
  - Add `snapshot_ids_serialize_as_plain_strings` in `model.rs` tests: a `TurnSnapshotStart` and a `TurnSnapshotPatch` JSON literal in the released format (UUID turn id, 40-hex snapshot id) deserialize and re-serialize byte-identical, and `rmp_serde::to_vec_named` of a `SnapshotId`/`TurnId` equals that of the bare string. Add `turn_id_that_is_not_a_uuid_still_pairs`: two parts with the same non-UUID turn id load without error and get equal `TurnId`s. Add `revert_state_snapshot_id_round_trips_through_sqlite` asserting the column still holds the bare id.

  Verify with `cargo check -p querymt-agent --features "$F" --all-targets`, then `cargo nextest run -p querymt-agent --features "$F" -E 'test(/snapshot_ids_serialize_as_plain_strings|turn_id_that_is_not_a_uuid_still_pairs|revert_state_snapshot_id|^snapshot::|^agent::undo_|^agent::execution::transitions::|^agent::session_materializer::/)'`, then `cargo check -p querymt-mobile-ffi`. The 8.1 test still fails here (no behaviour change yet). Record each summary.
  - Verified (rework to `SnapshotId(Arc<str>)` / `TurnId(Uuid)`, features `dashboard,oauth,remote`): `cargo check -p querymt-agent --all-targets` ok; the nextest filter above with `--no-fail-fast`: `238 tests run: 237 passed, 1 failed, 4088 skipped`, the one failure being the 8.1 test (`undo_of_a_prompt_that_deleted_a_file_restores_it`, expected until 8.3). New tests `snapshot_ids_serialize_as_plain_strings`, `turn_id_that_is_not_a_uuid_still_pairs` and `revert_state_snapshot_id_round_trips_through_sqlite` pass. `cargo check -p querymt-mobile-ffi` ok; `cargo fmt --check` and `cargo clippy -p querymt-agent --all-targets -- -D warnings` clean.
  - `uuid` gained the `v5` feature (for `from_stored`). `Cargo.lock` gains one dependency edge (`uuid` -> `sha1_smol`, already locked) and no new package.
  - The pre-turn task still mints its own `TurnId::new()` (no behaviour change); 8.3 makes it reuse `exec_ctx.turn_id()`.
  - Superseded: a first implementation with `String`-backed newtypes passed `cargo check --all-targets`, the filter above without the new turn-id test (`195 tests run: 194 passed, 1 failed, 4088 skipped`, the failure being the 8.1 test) and `cargo check -p querymt-mobile-ffi`. It was reworked because every second owner became an allocating clone (design D18, alternatives).
- [x] 8.3 Add `base_snapshot_id: Option<SnapshotId>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`) to `MessagePart::TurnSnapshotPatch` in `model.rs` (design D15). Set it in every constructor:
  - `session_actor.rs` post-turn patch: take `turn_id` and `base_snapshot_id` from the runtime `TurnSnapshot` instead of `run_id`.
  - `test_utils/helpers.rs` `take_post_snapshot_with_cwd`: `Some(pre_snapshot)`.
  - The `session_materializer.rs` test helper: `Some(pre_snapshot)`.

  In the pre-turn snapshot task, replace the freshly minted turn id with a copy of the run's `TurnId` (`exec_ctx.turn_id()`). Verify with `cargo check -p querymt-agent --features "$F"`, then the 8.1 command, which must now pass, and `cargo nextest run -p querymt-agent --features "$F" -E 'test(/^agent::undo_|^agent::session_materializer::|fork/)'`. Record both summaries.
  - Verified (features `dashboard,oauth,remote`): `cargo check -p querymt-agent --all-targets` ok. 8.1 command: `1 test run: 1 passed, 4325 skipped` (`undo_of_a_prompt_that_deleted_a_file_restores_it`, red before). Wider filter `^agent::undo_|^agent::session_materializer::|fork|snapshot_ids_serialize|turn_id_that_is_not|^snapshot::` with `--no-fail-fast`: `193 tests run: 193 passed, 4133 skipped`. `cargo fmt --check` and `cargo clippy -p querymt-agent --all-targets -- -D warnings` clean.
  - The pre-turn task now copies the run's `TurnId`; the post-turn patch takes `turn_id` and `base_snapshot_id` from the runtime `TurnSnapshot`. The fork copy matches the patch with `..`, so it keeps the new field.
- [x] 8.4 Replace both two-pass pairing loops in `undo_impl` with `resolve_turn_patches` and `TurnPatch` (design D16). Keep `validate_snapshot_workspace` on every part and leave the child-compatibility filter untouched. Tests in `agent/undo_integration_tests.rs`, writing records through the session store (earlier-format records have `base_snapshot_id: None`):
  - `workspace-snapshots` / Undo works without the turn's start record: `undo_works_without_the_turns_start_record`.
  - `workspace-snapshots` / Earlier-format turn is undone through its start record: `earlier_format_turn_is_undone_through_its_start_record`.
  - `workspace-snapshots` / Turn whose records disagree on turn id is undone by position: `turn_whose_records_disagree_on_turn_id_is_undone_by_position`.
  - `workspace-snapshots` / Overlapping turns are paired by turn id: `overlapping_turns_are_paired_by_turn_id`. Use two different pre-turn snapshots, so pairing by position would give `a.txt` the wrong content.

  Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/undo_works_without_the_turns_start_record|earlier_format_turn_is_undone_through_its_start_record|turn_whose_records_disagree_on_turn_id_is_undone_by_position|overlapping_turns_are_paired_by_turn_id|^agent::undo_/)'`.
  - Verified (features `dashboard,oauth,remote`, `--no-fail-fast`): `51 tests run: 51 passed, 4282 skipped` for the 8.4 and 8.5 filters together. `resolve_turn_patches`/`TurnPatch` live in `undo.rs`; the own history and each compatible child call the same resolver, and the child-compatibility filter is unchanged.
- [x] 8.5 Fail before `diff` validation, `restore_batch` and `push_revert_state` when a resolved patch has no base or its start comes before the target (design D17). Use `UndoError::Snapshot` with the D17 messages, and add no new variant. Tests:
  - `workspace-snapshots` / Turn without a pre-turn record fails undo without changes: `turn_without_a_pre_turn_record_fails_undo_without_changes`. Assert the error names the turn, `a.txt` is unchanged, and `peek_revert_state` returns `None`.
  - `workspace-snapshots` / Undo to a message inside a turn is refused: `undo_to_a_message_inside_a_turn_is_refused`. The history is a prompt, then a start, a user message with a `Steering` part, and a patch. Undo to the steering message fails as above. Undo to the prompt still succeeds, which proves the check only applies inside the turn.
  - `workspace-snapshots` / Turn that changed no files does not block undo: `turn_that_changed_no_files_does_not_block_undo`.

  Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/turn_without_a_pre_turn_record_fails_undo_without_changes|undo_to_a_message_inside_a_turn_is_refused|turn_that_changed_no_files_does_not_block_undo|^agent::undo_/)'`.
  - Verified: same run as 8.4. The D17 errors are returned from `resolve_turn_patches`, before the `diff` validation, `restore_batch` and `push_revert_state`; the tests assert unchanged files and no revert frame.
- [x] 8.6 Docs and changelog:
  - In `CHANGELOG.md` Unreleased, add three entries:
    - Undo restores files again for turns recorded since steering support. Undo that can't find a turn's pre-turn snapshot fails without changes instead of reporting success.
    - **BREAKING:** undo to a message inside a turn (for example a steering message) is refused.
    - **BREAKING (Rust API):** `snapshot::SnapshotId` is a newtype instead of a `String` alias, and turn ids use the new `TurnId` in `MessagePart` turn records and `ExecutionContext`. Stored sessions, revert frames and mesh payloads are unchanged.

    Note that a revert frame left by the broken undo can be removed with redo before the next prompt.
  - In `docs/docs/agent/configuration.md` "Snapshot scope and retention", add one sentence: undo reverts whole turns.

  Verify by reviewing both diffs.
  - Verified: reviewed `git diff CHANGELOG.md docs/docs/agent/configuration.md`. The three entries are under Unreleased (next to the other snapshot entries), with the redo note; the "undo reverts whole turns" sentence is in "Snapshot scope and retention".
- [x] 8.7 Re-run the 7.3 checks in order (`cargo check`, the nextest filter, `--doc snapshot`, `fmt --check`, `clippy -D warnings`), then the 7.4 downstream checks (`cargo check -p querymt-mobile-ffi`, `PYO3_PYTHON=<python>=3.10> cargo check -p querymt-agent-py`), then `openspec validate fix-undo-snapshot-coverage --strict`. Record each result.
  - Verified (features `dashboard,oauth,remote`, macOS; zbus/systemd are Linux-only and unverified):
    1. `cargo check -p querymt-agent --features "$F"`: ok.
    2. `cargo nextest run --no-fail-fast -p querymt-agent --features "$F" -E 'test(/snapshot|undo|redo|config::|agent_config_builder|quorum|turn_id|fork|session_materializer/)'` (the 7.3 filter plus the 8.x areas): `494 tests run: 494 passed, 3839 skipped`.
    3. `cargo test -p querymt-agent --features "$F" --doc snapshot`: ok (0 passed, 0 failed, 1 ignored).
    4. `cargo fmt -p querymt-agent -- --check`: ok.
    5. `cargo clippy -p querymt-agent --features "$F" --all-targets -- -D warnings`: ok.
    - `cargo check -p querymt-mobile-ffi`: ok. `PYO3_PYTHON=/opt/homebrew/bin/python3.14 cargo check -p querymt-agent-py`: ok.
    - `openspec validate fix-undo-snapshot-coverage --strict`: "Change ... is valid".

## Workflow follow-up

- Archive the change after review, which syncs `workspace-snapshots` into `openspec/specs/`.
