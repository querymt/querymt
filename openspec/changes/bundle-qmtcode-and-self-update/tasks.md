# Tasks

Verification commands:

- **querymt-gpui tests:** `cargo t-gpui -E 'test(/<name>/)'`, an alias for `cargo nextest run -p querymt-gpui --bins`.
- **Clippy:** `cargo cl-gpui`.
- **Format:** `cargo fmt -p querymt-gpui -- --check`.
- **wasm check:** `RUSTC_BOOTSTRAP=1 cargo check -p querymt-gpui --target wasm32-unknown-unknown`.
- **Release scripts:** `scripts/release/test-release-scripts.sh`, which also runs `crates/querymt-gpui/scripts/release/test-release-scripts.sh`.

File paths below `src/`, `scripts/` and `packaging/` are relative to `crates/querymt-gpui` unless they start with the repository root (`.github/`, `scripts/release/`, `flake.nix`).

## 1. Local qmtcode resolution and seeding (app)

- [ ] 1.1 Add `src/local_agent.rs` (native only) with `bundled_qmtcode`, `find_on_path` and `resolve_program` over injected exe path and PATH value (design D4). Tests use temp directories and never change the process environment.
  - Covers:
    - desktop-local-agent / Bundled copy wins over PATH
    - desktop-local-agent / PATH copy is used without a bundled copy
    - desktop-local-agent / Linux lib layout is found
    - desktop-local-agent / Windows finds qmtcode.exe (test on `cfg(windows)`; on other hosts the same test checks the suffix logic through an injected suffix)
    - desktop-local-agent / No qmtcode anywhere
    - desktop-local-agent / Path to qmtcode is not replaced
    - desktop-local-agent / Other programs are unaffected
  - Tests: `bundled_copy_wins_over_path`, `path_copy_is_used_without_a_bundled_copy`, `linux_lib_layout_is_found`, `windows_finds_qmtcode_exe`, `no_qmtcode_anywhere`, `path_to_qmtcode_is_not_replaced`, `other_programs_are_unaffected`.
  - Verify: `cargo t-gpui -E 'test(/bundled_copy_wins|path_copy_is_used|linux_lib_layout|windows_finds_qmtcode|no_qmtcode_anywhere|path_to_qmtcode_is_not|other_programs_are/)'` passes.
- [ ] 1.2 In `src/transport.rs` `native::spawn_process`:
  - Resolve the program with `resolve_program` before `Command::new`.
  - Write the start note with the source label, the full path and the pid.
  - Set `CREATE_NO_WINDOW` on Windows.
  - Covers:
    - desktop-local-agent / Log shows bundled copy
    - desktop-local-agent / Log shows PATH copy
    - desktop-local-agent / No console window for a local agent
  - Tests:
    - `log_shows_bundled_copy` and `log_shows_path_copy`: a fake `qmtcode` script in a temp dir that echoes and exits. Spawn through the production path with injected exe and PATH, and assert the `Inbound::Note` text. Unix only.
    - `no_console_window_for_a_local_agent`: on `cfg(windows)`, asserts the creation flags the command builder sets, and that stdin, stdout and stderr are piped.
  - Verify:
    - `cargo t-gpui -E 'test(/log_shows_bundled_copy|log_shows_path_copy/)'` passes on macOS.
    - The Windows test runs in test-gpui.yml's Windows leg, or, if that leg does not exist, by running `cargo t-gpui -E 'test(/no_console_window/)'` on a Windows machine. Record which here.
- [ ] 1.3 Seeding and dismissal (design D5):
  - `seed_configs` uses `bundled_qmtcode` and `find_on_path`.
  - `Prefs.dismissed_seeds` has a serde default.
  - `AgentsStore::new` skips dismissed seeds.
  - `delete_config` records `local-qmtcode`.
  - `reset_app_data` clears the list.
  - Covers:
    - desktop-local-agent / Bundled qmtcode seeds the local agent
    - desktop-local-agent / Windows PATH qmtcode seeds the local agent
    - desktop-local-agent / Nothing to seed without qmtcode
    - desktop-local-agent / Existing installs gain the local agent
    - desktop-local-agent / Deleted local agent does not return
    - desktop-local-agent / Reset app data restores the local agent
    - desktop-local-agent / Requested agent returns after deletion
    - desktop-local-agent / Older state files load without dismissals
  - Tests: `bundled_qmtcode_seeds_the_local_agent`, `windows_path_qmtcode_seeds_the_local_agent`, `nothing_to_seed_without_qmtcode`, `existing_installs_gain_the_local_agent`, `deleted_local_agent_does_not_return`, `reset_app_data_restores_the_local_agent`, `requested_agent_returns_after_deletion`, `older_state_files_load_without_dismissals`. Seed inputs are injected; the store tests use the GPUI test context.
  - Verify: `cargo t-gpui -E 'test(/seeds_the_local_agent|nothing_to_seed|gain_the_local_agent|does_not_return|restores_the_local_agent|returns_after_deletion|load_without_dismissals/)'` passes.
- [ ] 1.4 Run the group checks and record the commands and counts here:
  - full `cargo t-gpui`;
  - `cargo fmt -p querymt-gpui -- --check`;
  - `cargo cl-gpui`;
  - the wasm check.

## 2. Packaging: bundled qmtcode and identity

- [ ] 2.1 `scripts/package/macos.sh` (design D2):
  - Bundle `QueryMT Desktop.app`, identifier `com.querymt.desktop`, name and display name "QueryMT Desktop".
  - Take `QMTCODE_BINARY`: copy it to `Contents/MacOS/qmtcode` and ad-hoc sign it before the bundle.
  - Without `QMTCODE_BINARY`: error under `CI`, otherwise a warning.
  - Covers: desktop-packaging / macOS bundle identity, and the macOS part of desktop-packaging / macOS app bundle contains qmtcode.
  - Verify: run `QMTCODE_BINARY=target/release/examples/qmtcode bash crates/querymt-gpui/scripts/package/macos.sh 0.0.0 aarch64-apple-darwin` locally after `cargo build -p querymt-gpui --profile release-gpui --target aarch64-apple-darwin`. Then check:
    - `plutil -p` shows the identifier;
    - `codesign --verify --strict` passes;
    - `Contents/MacOS/qmtcode --version` runs.

    Record the output here.
- [ ] 2.2 `scripts/package/linux.sh` and `packaging/nfpm.yaml`:
  - qmtcode goes to `usr/lib/querymt-gpui/qmtcode` in the AppDir and in `PKGROOT`.
  - Package name `querymt-desktop`, with `replaces`, `conflicts` and `provides` for `querymt-gpui`.
  - The `.desktop` file gets `Name=QueryMT Desktop`.
  - Same `QMTCODE_BINARY` rule as 2.1.
  - Covers: desktop-packaging / Linux packages install qmtcode outside PATH, and desktop-packaging / Deb replaces the earlier package name. The deb metadata is checked by 2.5; the replacement itself is checked manually in 9.2.
  - Verify: `bash -n` and `shellcheck` on the script; the real run happens in CI (group 3).
- [ ] 2.3 `scripts/package/windows.sh` and `packaging/windows/installer.nsi`:
  - Required `QMTCODE_EXE` define, installed as `$INSTDIR\qmtcode.exe` and deleted on uninstall.
  - Name "QueryMT Desktop", install directory `$LOCALAPPDATA\Programs\QueryMT Desktop`, uninstall key `QueryMTDesktopGpui`.
  - Silent removal of an existing `querymt-gpui` install.
  - `/UPDATE` mode: wait up to 30 s for the exe to be free, install silently, relaunch.
  - Covers:
    - desktop-packaging / Windows installer installs qmtcode next to the app
    - desktop-packaging / Windows uninstaller removes qmtcode
    - desktop-packaging / Windows installer replaces an earlier GPUI install

    The `/UPDATE` mode is exercised by 8.4.
  - Verify: `makensis -V3` compiles the script with dummy inputs on the Windows CI leg (group 3); the install, uninstall and replacement behaviour is checked manually in 9.2.
- [ ] 2.4 Make sure user state is keyed by `state_path` (`querymt-gpui/state.json`) and nothing in the rename touches it.
  - Covers: desktop-packaging / User state survives the rename.
  - Test: `user_state_survives_the_rename` asserts that `state_path()` ends in `querymt-gpui/state.json`, independent of bundle name or identifier.
  - Verify: `cargo t-gpui -E 'test(/user_state_survives_the_rename/)'` passes.
- [ ] 2.5 Add `scripts/release/check-bundles.sh` (design D3) and extend `scripts/release/test-release-scripts.sh` with fake bundles: a directory tree for `.app`, file lists for deb, AppImage and NSIS, with the listing commands stubbed through env overrides.
  - Covers:
    - desktop-packaging / Installer content check fails the build
    - desktop-packaging / CI packaging without qmtcode fails
    - desktop-packaging / Local packaging without qmtcode warns

    The packaging-script half of the last two is a helper shared by the three scripts; it is tested here by sourcing it.
  - Tests: `installer content check fails the build`, `ci packaging without qmtcode fails`, `local packaging without qmtcode warns` (case names in the shell test).
  - Verify: `scripts/release/test-release-scripts.sh` passes and prints those case names as ok.
- [ ] 2.6 Update the README "QueryMT Desktop" section and `crates/querymt-gpui/README.md`: bundled qmtcode, install locations, the new identity, and that earlier GPUI test installs on macOS are removed by hand.
  - Verify: the README steps match the scripts (review).

## 3. CI: compile and package jobs

- [ ] 3.1 `.github/workflows/build.yml` (design D1):
  - `desktop` uploads `desktop-bin-<target>` instead of packaging.
  - New `desktop-package` job (`needs: [desktop, build]`) with the `qmtcode_target` matrix field. It downloads both artifacts, unpacks the qmtcode archive (tar.gz with directory, or zip at root), sets `QMTCODE_BINARY` and runs the packaging script, then `check-bundles.sh`, then `collect-bundles.sh`, and uploads `qmt-desktop-<target>`.
  - Linux legs install `dpkg-dev`; Windows uses the preinstalled 7-Zip.
  - Covers: desktop-packaging / macOS app bundle contains qmtcode, Bundled qmtcode matches the release version, and Installer content check fails the build, all in CI.
  - Verify: `actionlint .github/workflows/build.yml` is clean, and a `workflow_dispatch` nightly run (9.1) passes `desktop-package` on all five targets.
- [ ] 3.2 Check that `collect-bundles.sh` still collects the renamed bundles (`QueryMT Desktop.app.tar.gz`, the dmg) and update its fixtures in `test-release-scripts.sh`.
  - Verify: `scripts/release/test-release-scripts.sh` passes.

## 4. Nix

- [ ] 4.1 `flake.nix`:
  - Move the qmtcode package into the `let` block.
  - `querymtGpui` `postInstall` symlinks `$out/lib/querymt-gpui/qmtcode` on all platforms (design D2).
  - `packages.qmtcode` refers to the `let` binding.
  - Covers: desktop-packaging / Nix package exposes qmtcode to the app.
  - Verify:
    - format with alejandra;
    - `nix build .#querymt-gpui && test -x result/lib/querymt-gpui/qmtcode && result/lib/querymt-gpui/qmtcode --version` (on a machine with Nix, or in `nix.yml` CI).

    Record where it ran. If Nix is unavailable locally, leave this unticked until CI runs it.

## 5. Signed release manifest

- [ ] 5.1 Create the minisign key pair (no password for the CI key):
  - Commit the public key as `crates/querymt-gpui/packaging/update-key.pub`.
  - Add the secret key to the repository secret `QMT_UPDATE_SIGNING_KEY`.
  - Store an offline backup.
  - Verify: `minisign -Vm <file> -p crates/querymt-gpui/packaging/update-key.pub` verifies a test file signed with the key. The user confirms the secret exists (needs repository admin; ask the user).
- [ ] 5.2 `scripts/release/write-release-json.sh`: named flags `--desktop-dir` and `--version-name` (D7).
  - Writes `desktop_updates` with file, size and SHA-256 for the five platform keys, and fails when any file is missing.
  - The existing fields stay unchanged.
  - Coordinate the flag names with `add-agent-session-core` if it has landed.
  - Covers:
    - desktop-self-update / Manifest lists every self-updatable platform
    - desktop-self-update / Manifest keeps the existing fields
    - desktop-self-update / Missing asset or key fails the release (missing-asset half)
  - Tests in `scripts/release/test-release-scripts.sh`: `manifest lists every self-updatable platform`, `manifest keeps the existing fields`, `missing desktop asset fails`.
  - Verify: `scripts/release/test-release-scripts.sh` passes.
- [ ] 5.3 Add `scripts/release/sign-release-json.sh <release.json> <version> <channel>`. It reads the key from `QMT_UPDATE_SIGNING_KEY`, writes `release.json.minisig` with trusted comment `version:<v> channel:<c>`, and fails without the key.
  - Add a step in `nightly.yml` and `release.yml` after "Write release.json" that installs `minisign` and runs it.
  - Covers:
    - desktop-self-update / Signature covers the manifest
    - desktop-self-update / Missing asset or key fails the release (key half)
  - Tests in the release-script tests: `signature covers the manifest` (a throwaway key generated in the test, verified with `minisign -V` and the trusted comment checked), `missing signing key fails`.
  - Verify: `scripts/release/test-release-scripts.sh` passes; `actionlint` is clean.

## 6. Updater core (pure logic)

- [ ] 6.1 Add dependencies (design D13): `ureq` 3 (rustls, no default features), `minisign-verify`, `semver`, `sha2`, and `tar` + `flate2` (macOS), all under native target tables.
  - Verify:
    - `cargo tree -p querymt-gpui -e normal -i aws-lc-rs` reports nothing;
    - the wasm check still passes;
    - `Cargo.lock` only gains `minisign-verify` and new edges.
- [ ] 6.2 `build.rs` emits `QMT_UPDATE_CHANNEL` (D6). `src/update/mod.rs` has the channel from the build and from preferences.
  - Covers: desktop-self-update / Nightly build follows the nightly channel, and the channel half of Development build makes no update request.
  - Tests: `nightly_build_follows_the_nightly_channel`, plus a build-channel table test `build_channel_from_version` (dev, nightly, stable).
  - Verify: `cargo t-gpui -E 'test(/nightly_build_follows|build_channel_from_version/)'` passes.
- [ ] 6.3 `src/update/verify.rs`: verify the manifest against the embedded key and trusted comment (D7), and check the asset's size and SHA-256. Fixtures are signed in the test with a throwaway key; the production key is a `const` that tests can replace.
  - Covers:
    - desktop-self-update / Bad signature is refused
    - desktop-self-update / Signature for another version is refused
    - desktop-self-update / Corrupted download is rejected (verification half)
    - desktop-self-update / Nightly replaced during download (verification half)
  - Tests: `bad_signature_is_refused`, `signature_for_another_version_is_refused`, `corrupted_download_is_rejected`, `nightly_replaced_during_download`.
  - Verify: `cargo t-gpui -E 'test(/bad_signature_is_refused|signature_for_another_version|corrupted_download_is_rejected|nightly_replaced_during_download/)'` passes.
- [ ] 6.4 `evaluate`, `is_suppressed` and the channel check.
  - Covers:
    - desktop-self-update / Newer signed version is offered
    - desktop-self-update / Same or older version is not offered
    - desktop-self-update / Switching to stable never downgrades
    - desktop-self-update / Skipped version stays quiet
    - desktop-self-update / Manual check shows skipped version
  - Tests: `newer_signed_version_is_offered`, `same_or_older_version_is_not_offered`, `switching_to_stable_never_downgrades`, `skipped_version_stays_quiet`, `manual_check_shows_skipped_version`.
  - Verify: `cargo t-gpui -E 'test(/newer_signed_version|same_or_older_version|switching_to_stable|skipped_version_stays_quiet|manual_check_shows_skipped/)'` passes.
- [ ] 6.5 `detect_install_kind` (D9).
  - Covers (the kind half; the UI half is in 7.3):
    - desktop-self-update / Deb install is notify-only
    - desktop-self-update / Translocated macOS app is notify-only
  - Tests: `deb_install_is_notify_only`, `translocated_macos_app_is_notify_only`, plus `install_kind_table` (writable `.app`, NSIS location match, AppImage writable and read-only, Nix store).
  - Verify: `cargo t-gpui -E 'test(/deb_install_is_notify_only|translocated_macos_app|install_kind_table/)'` passes.
- [ ] 6.6 Scheduling (`check_due`, single check in flight) and `UpdatePrefs` with serde defaults.
  - Covers:
    - desktop-self-update / First automatic check after start
    - desktop-self-update / Automatic checks off
    - desktop-self-update / Concurrent check is ignored
    - desktop-self-update / Older state loads with update defaults
  - Tests: `first_automatic_check_after_start`, `automatic_checks_off`, `concurrent_check_is_ignored` (drive `check_due` with an injected clock), `older_state_loads_with_update_defaults`.
  - Verify: `cargo t-gpui -E 'test(/first_automatic_check|automatic_checks_off|concurrent_check_is_ignored|older_state_loads_with_update_defaults/)'` passes.
- [ ] 6.7 Run the group checks and record the commands and counts here:
  - full `cargo t-gpui`;
  - fmt;
  - `cargo cl-gpui`;
  - the wasm check.

## 7. Updater runtime: fetch, store and UI

- [ ] 7.1 `src/update/fetch.rs`: a ureq agent with timeouts and proxy settings from the environment, fetching the manifest and its signature. Downloads run on a dedicated thread and report progress over a channel. The base URL is injectable for tests.
  - Covers (fetch half):
    - desktop-self-update / Automatic check offline
    - desktop-self-update / Manual check offline
  - Tests against a local `std::net::TcpListener` HTTP stub:
    - `automatic_check_offline` and `manual_check_offline`: an unreachable port gives a network failure, and the state reaches a log-only error (automatic) or a shown error (manual);
    - `download_reports_progress`.
  - Verify: `cargo t-gpui -E 'test(/automatic_check_offline|manual_check_offline|download_reports_progress/)'` passes.
- [ ] 7.2 `src/store/updates.rs`: `UpdateState`, a scheduler started from `AgentsStore::new` (none for `dev`), `check_updates`, `skip_version`, `install_update`, `restart_to_update`, and persisted preferences (D8).
  - Covers:
    - desktop-self-update / Development build makes no update request
    - desktop-self-update / No download without consent
    - desktop-self-update / Preferences survive restart
  - Tests (GPUI test context with the HTTP stub; the request counter is read from the stub):
    - `development_build_makes_no_update_request`;
    - `no_download_without_consent` (after a check finds an update, the stub saw no asset request);
    - `preferences_survive_restart` (save, reload `Persisted`).
  - Verify: `cargo t-gpui -E 'test(/development_build_makes_no_update_request|no_download_without_consent|preferences_survive_restart/)'` passes.
- [ ] 7.3 Read the gpui-kit-design-guides skill. Then add the Updates card in `src/ui/settings.rs` `render_settings` and the sidebar "Update available" item (D12).
  - Covers (UI: version, release link and the move hint, no install action):
    - desktop-self-update / Deb install is notify-only
    - desktop-self-update / Translocated macOS app is notify-only
  - Tests: `notify_only_shows_release_link` and `translocated_shows_move_hint`, GPUI view tests using the debug selectors the settings tests already use.
  - Verify: `cargo t-gpui -E 'test(/notify_only_shows_release_link|translocated_shows_move_hint/)'` passes; screenshot review against the design guides.

## 8. Updater runtime: install and relaunch

- [ ] 8.1 Stop local process agents before replacing files: a store method that disconnects every `Endpoint::Process` agent and waits for the child to exit, with a timeout.
  - Covers: desktop-self-update / Local agents stop before files change.
  - Test: `local_agents_stop_before_files_change`. A fake agent script records its exit time and the install step records its first rename; assert the order.
  - Verify: `cargo t-gpui -E 'test(/local_agents_stop_before_files_change/)'` passes (Unix).
- [ ] 8.2 `src/update/install/macos.rs` (D10): unpack into a sibling temp dir, reject unsafe entries, check the bundle, rename with rollback, clean up.
  - Covers:
    - desktop-self-update / macOS bundle is replaced in place
    - desktop-self-update / Failed install keeps the old version (macOS)
    - desktop-self-update / Corrupted download is rejected (cleanup half)
  - Tests on temp dirs with a fake bundle tarball; `codesign` is injectable and skipped in tests:
    - `macos_bundle_is_replaced_in_place`;
    - `failed_install_keeps_the_old_version`, with the second rename failing through an injected fault;
    - `corrupted_download_leaves_no_files`.
  - Verify: `cargo t-gpui -E 'test(/macos_bundle_is_replaced_in_place|failed_install_keeps_the_old_version|corrupted_download_leaves_no_files/)'` passes.
- [ ] 8.3 `src/update/install/appimage.rs` (D10): sibling temp file, chmod, rename over `$APPIMAGE`.
  - Covers: desktop-self-update / AppImage is replaced in place.
  - Test: `appimage_is_replaced_in_place`, on temp files, asserting the path's content and mode 0755.
  - Verify: `cargo t-gpui -E 'test(/appimage_is_replaced_in_place/)'` passes.
- [ ] 8.4 `src/update/install/windows.rs` (D11): download the setup to `%TEMP%` and spawn it detached with `/S /UPDATE /D=<dir>`. Also the restart path, `restart_to_update`: stop agents, drop the instance listener, spawn the new app, quit.
  - Covers:
    - desktop-self-update / Windows installer runs after the app exits
    - desktop-self-update / Restart opens the new version
  - Tests:
    - `windows_installer_runs_after_the_app_exits`: builds the argument list, `/D` last and unquoted (pure test).
    - `restart_opens_the_new_version`: asserts the order "listener dropped, then child spawned, then quit" through injected effects, and that a second `forward` after the drop reaches nobody (reuses `quick::instance` test helpers).
  - Verify: `cargo t-gpui -E 'test(/windows_installer_runs_after|restart_opens_the_new_version/)'` passes. The real Windows run is in 9.2.
- [ ] 8.5 Run the group checks and record the commands and counts here:
  - full `cargo t-gpui`;
  - fmt;
  - `cargo cl-gpui`;
  - the wasm check.

## 9. End-to-end checks

- [ ] 9.1 Run the nightly workflow with `workflow_dispatch` on this branch's merge commit. Check that:
  - `desktop-package` passes on all five targets;
  - the release has `release.json` with `desktop_updates`, and `release.json.minisig`;
  - `minisign -Vm release.json -p crates/querymt-gpui/packaging/update-key.pub` verifies;
  - each listed SHA-256 matches the downloaded asset.

  Covers: desktop-self-update / Manifest lists every self-updatable platform (end to end), and desktop-packaging / Bundled qmtcode matches the release version (end to end). Record the run URL here.
- [ ] 9.2 Manual install checks on real machines, from the 9.1 assets. Record the results here.
  - **Windows:**
    - Install over an earlier "QueryMT GPUI" install: only "QueryMT Desktop" remains, and `qmtcode.exe` is next to the app.
    - Uninstall: both are gone.
    - No console window when the local agent starts.
  - **Ubuntu:** `apt install ./…deb` over `querymt-gpui`: the old package is replaced and `/usr/lib/querymt-gpui/qmtcode` exists.
  - **macOS:** the app starts the bundled qmtcode with no qmtcode on PATH, and the agent log says "bundled".

  Covers: desktop-packaging / Windows installer installs qmtcode next to the app, Windows uninstaller removes qmtcode, Windows installer replaces an earlier GPUI install, Deb replaces the earlier package name, and desktop-local-agent / No console window for a local agent (manual).
- [ ] 9.3 Update across two nightlies on macOS (arm64), Windows x64 and an AppImage. Install nightly N, publish N+1, then:
  - the app offers N+1;
  - installing works;
  - "Restart now" opens N+1 once;
  - the local agent comes back.

  Also check that macOS starts the updated bundle without a Gatekeeper prompt (design Risks). Covers: desktop-self-update / Restart opens the new version, Windows installer runs after the app exits, macOS bundle is replaced in place, and AppImage is replaced in place (end to end). Record the results here.
- [ ] 9.4 Final checks; record each command and result here:
  - `cargo t-gpui`;
  - `cargo cl-gpui`;
  - `cargo fmt --all --check`;
  - the wasm check;
  - `cargo hakari verify`;
  - `scripts/release/test-release-scripts.sh`;
  - `actionlint` on the changed workflows;
  - `openspec validate bundle-qmtcode-and-self-update --strict`.

  No querymt-agent code changes; confirm that with `git diff --stat -- crates/agent crates/agent-*`.

## Workflow follow-up

- The follow-up change retires the Tauri app: final querymt-desktop release with a "moved" notice, removal of the nightly dispatch step in `nightly.yml`, and the Tauri profile import question.
- Archive this change after 9.1–9.3 are recorded.
