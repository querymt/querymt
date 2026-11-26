# Tasks

Verification commands:

- **querymt-gpui tests:** `cargo t-gpui -E 'test(/<name>/)'`, an alias for `cargo nextest run -p querymt-gpui --bins`.
- **Clippy:** `cargo cl-gpui`.
- **Format:** `cargo fmt -p querymt-gpui -- --check`.
- **wasm check:** `RUSTC_BOOTSTRAP=1 cargo check -p querymt-gpui --target wasm32-unknown-unknown`.
- **Release scripts:** `scripts/release/test-release-scripts.sh`, which also runs `crates/querymt-gpui/scripts/release/test-release-scripts.sh`.

File paths below `src/`, `scripts/` and `packaging/` are relative to `crates/querymt-gpui` unless they start with the repository root (`.github/`, `scripts/release/`, `flake.nix`).

## 1. Local qmtcode resolution and seeding (app)

- [x] 1.1 Add `src/local_agent.rs` (native only) with `bundled_qmtcode`, `find_on_path` and `resolve_program` over injected exe path and PATH value (design D4). Tests use temp directories and never change the process environment.
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
- [x] 1.2 In `src/transport.rs` `native::spawn_process`:
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
- [x] 1.3 Seeding and dismissal (design D5):
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
- [x] 1.4 Run the group checks and record the commands and counts here:
  - full `cargo t-gpui`;
  - `cargo fmt -p querymt-gpui -- --check`;
  - `cargo cl-gpui`;
  - the wasm check.

  Recorded (macOS arm64, cargo-nextest 0.9.136):
  - `cargo t-gpui`: 614 run, 614 passed, 5 skipped.
  - `cargo fmt -p querymt-gpui -- --check`: clean.
  - `cargo cl-gpui`: clean (no warnings).
  - `RUSTC_BOOTSTRAP=1 cargo check -p querymt-gpui --target wasm32-unknown-unknown`: passes; one `tip_with_key` dead-code warning that also appears without this group's changes.
  - 1.1 filter: 8 of 8 local_agent tests pass. 1.2 filter: `log_shows_bundled_copy`, `log_shows_path_copy` pass on macOS, plus `explicit_path_is_logged_as_written` and `missing_qmtcode_fails_to_start`. 1.3 filter: 8 of 8 pass.
  - `no_console_window_for_a_local_agent` is `cfg(windows)` and has not run: `test-gpui.yml` has no Windows leg, so it needs `cargo t-gpui -E 'test(/no_console_window/)'` on a Windows machine. `std::process::Command` cannot read back its creation flags, so the test checks the flag constant and that the production builder pipes stdin, stdout and stderr; `CREATE_NO_WINDOW` itself is checked by review and by the manual Windows check in 9.2.
  - Deviations from the task text: `bundled_qmtcode` and `find_on_path` are private helpers taking an injected exe suffix (a public no-suffix wrapper would be dead code under `-D warnings`); `local_agent::available_with_suffix` is what `seed_configs` uses. `Prefs`-level startup logic moved into a pure `startup_state` so the seeding scenarios test without reading the user's state file.

## 2. Packaging: bundled qmtcode and identity

- [ ] 2.1 `scripts/package/macos.sh` (design D2):
  - Bundle `QueryMT Desktop.app`, identifier `mt.query.desktop`, name and display name "QueryMT Desktop".
  - Take `QMTCODE_BINARY`: copy it to `Contents/MacOS/qmtcode` and ad-hoc sign it before the bundle.
  - Without `QMTCODE_BINARY`: error under `CI`, otherwise a warning.
  - Covers: desktop-packaging / macOS bundle identity, and the macOS part of desktop-packaging / macOS app bundle contains qmtcode.
  - Verify: run `QMTCODE_BINARY=target/release/examples/qmtcode bash crates/querymt-gpui/scripts/package/macos.sh 0.0.0 aarch64-apple-darwin` locally after `cargo build -p querymt-gpui --profile release-gpui --target aarch64-apple-darwin`. Then check:
    - `plutil -p` shows the identifier;
    - `codesign --verify --strict` passes;
    - `Contents/MacOS/qmtcode --version` runs.

    Record the output here.

  Status (open): the script is done and was run on macOS arm64 with stand-ins, because no real
  `release-gpui` app or qmtcode build exists on this machine (both are long builds):
  copies of `/bin/echo` for `querymt-gpui` and `qmtcode`, `CARGO_TARGET_DIR=/tmp/pkgtest`,
  `QMTCODE_BINARY=<stand-in> bash crates/querymt-gpui/scripts/package/macos.sh 0.0.0 aarch64-apple-darwin`.
  - `plutil -p`: `CFBundleIdentifier` is `mt.query.desktop`; `CFBundleName` and `CFBundleDisplayName` are `QueryMT Desktop`.
  - `codesign --verify --strict "QueryMT Desktop.app"` passes; `Contents/MacOS/qmtcode` exists and is signed first.
  - `QueryMT Desktop.app.tar.gz` and the dmg are written, and `check-bundles.sh` opened the dmg with hdiutil and found qmtcode (with `CHECK_HOST_ARCH` set to skip running it).
  - `CI=1` without `QMTCODE_BINARY` exits 1 and leaves the bundle directory untouched.
  - Still to do: the run with the real qmtcode (`Contents/MacOS/qmtcode --version`); tick this task once it has been done, or in the first CI run (3.1 / 9.1).
- [x] 2.2 `scripts/package/linux.sh` and `packaging/nfpm.yaml`:
  - qmtcode goes to `usr/lib/querymt-gpui/qmtcode` in the AppDir and in `PKGROOT`.
  - Package name `querymt-desktop`, with `replaces`, `conflicts` and `provides` for `querymt-gpui`.
  - The `.desktop` file gets `Name=QueryMT Desktop`.
  - Same `QMTCODE_BINARY` rule as 2.1.
  - Covers: desktop-packaging / Linux packages install qmtcode outside PATH, and desktop-packaging / Deb replaces the earlier package name. The deb metadata is checked by 2.5; the replacement itself is checked manually in 9.2.
  - Verify: `bash -n` and `shellcheck` on the script; the real run happens in CI (group 3).

  Recorded: `bash -n` passes; `shellcheck -x -P SCRIPTDIR -S warning scripts/package/*.sh scripts/release/*.sh` is clean (shellcheck 0.11.0). `linux.sh` was also run with stub `appimagetool` and `nfpm`: qmtcode lands in `QueryMT.AppDir/usr/lib/querymt-gpui/` and `pkgroot/usr/lib/querymt-gpui/` with mode 755, and `scripts/release/test-release-scripts.sh` covers the same run. The real appimagetool/nfpm run is in CI. Also fixed an empty-array expansion in `linux.sh` that failed under bash 3.2 (macOS) with `set -u`.
- [x] 2.3 `scripts/package/windows.sh` and `packaging/windows/installer.nsi`:
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

  Recorded: makensis 3.13 (Homebrew, macOS) compiles `installer.nsi` with and without `QMTCODE_EXE`, and `windows.sh` builds a setup exe through it with a stand-in app and qmtcode. `CI=1` without `QMTCODE_BINARY` exits 1 and writes nothing. The `/UPDATE` wait loop, the old-install removal and the uninstall have not run on Windows: they are checked in 8.4 / 9.2.
  Deviation: the spec says local packaging without qmtcode still writes the installer, so `QMTCODE_EXE` is optional in `installer.nsi` (a `!warning` and no qmtcode in the setup) rather than a required define. `windows.sh` omits it only where `resolve_qmtcode` allows it (not under CI), and `check-bundles.sh` fails any CI setup without `qmtcode.exe`.
  The `/UPDATE` mode tests for a free exe by opening it for append (the running app keeps it mapped) rather than by `Delete`, so a wait that times out changes nothing and the old version stays intact.
- [x] 2.4 Make sure user state is keyed by `state_path` (`querymt-gpui/state.json`) and nothing in the rename touches it.
  - Covers: desktop-packaging / User state survives the rename.
  - Test: `user_state_survives_the_rename` asserts that `state_path()` ends in `querymt-gpui/state.json`, independent of bundle name or identifier.
  - Verify: `cargo t-gpui -E 'test(/user_state_survives_the_rename/)'` passes.

  Recorded: passes (1 test). `state_path()` was not touched; the test pins it.
- [x] 2.5 Add `scripts/release/check-bundles.sh` (design D3) and extend `scripts/release/test-release-scripts.sh` with fake bundles: a directory tree for `.app`, file lists for deb, AppImage and NSIS, with the listing commands stubbed through env overrides.
  - Covers:
    - desktop-packaging / Installer content check fails the build
    - desktop-packaging / CI packaging without qmtcode fails
    - desktop-packaging / Local packaging without qmtcode warns

    The packaging-script half of the last two is a helper shared by the three scripts; it is tested here by sourcing it.
  - Tests: `installer content check fails the build`, `ci packaging without qmtcode fails`, `local packaging without qmtcode warns` (case names in the shell test).
  - Verify: `scripts/release/test-release-scripts.sh` passes and prints those case names as ok.

  Recorded: `scripts/release/test-release-scripts.sh` passes (61 `ok` lines, none failing). The case names above appear as prefixes of the ok lines (`installer content check fails the build: <what is wrong>` has seven variants over macOS, deb, AppImage and Windows); `ci packaging without qmtcode fails` and `local packaging without qmtcode warns` run the real `linux.sh` with stub tools, and the helper is tested by sourcing `common.sh`. The shared helper is `scripts/package/common.sh` (`resolve_qmtcode`). The macOS dmg branch of `check-bundles.sh` (hdiutil) is not in the shell tests; it was run by hand against the dmg from 2.1.
- [x] 2.6 Update the README "QueryMT Desktop" section and `crates/querymt-gpui/README.md`: bundled qmtcode, install locations, the new identity, and that earlier GPUI test installs on macOS are removed by hand.
  - Verify: the README steps match the scripts (review).

  Recorded: root README and `crates/querymt-gpui/README.md` updated (bundled qmtcode, lookup order, install locations, identity table, manual removal of the old macOS `QueryMT.app`, local packaging commands). Neither mentions self-update yet; that belongs to groups 6–8.

## 3. CI: compile and package jobs

- [ ] 3.1 `.github/workflows/build.yml` (design D1):
  - `desktop` uploads `desktop-bin-<target>` instead of packaging.
  - New `desktop-package` job (`needs: [desktop, build]`) with the `qmtcode_target` matrix field. It downloads both artifacts, unpacks the qmtcode archive (tar.gz with directory, or zip at root), sets `QMTCODE_BINARY` and runs the packaging script, then `check-bundles.sh`, then `collect-bundles.sh`, and uploads `qmt-desktop-<target>`.
  - Linux legs install `dpkg-dev`; Windows uses the preinstalled 7-Zip.
  - Covers: desktop-packaging / macOS app bundle contains qmtcode, Bundled qmtcode matches the release version, and Installer content check fails the build, all in CI.
  - Verify: `actionlint .github/workflows/build.yml` is clean, and a `workflow_dispatch` nightly run (9.1) passes `desktop-package` on all five targets.

  Status (open): the workflow is written; the CI run is the missing half of the Verify line.
  - `actionlint .github/workflows/build.yml` (1.7.12) reports exactly the five findings it reports on `HEAD`'s file, all in the untouched `build` job: the custom `freebsd` runner label (4) and an info-level SC2086 on `>> $GITHUB_ENV`. Nothing in `desktop` or `desktop-package`.
  - `desktop` now only compiles (the Linux build dependencies and the Metal toolchain stay; the packaging tools and NSIS moved) and uploads `desktop-bin-<target>`.
  - `desktop-package` (`needs: [desktop, build]`) has the `qmtcode_target` field (musl for Linux). It downloads both artifacts, restores the app's executable bit (artifacts do not keep modes), unpacks the qmtcode archive by extension (tar.gz, or zip through 7-Zip), exports an absolute `QMTCODE_BINARY` (the packaging scripts change directory), packages, runs `check-bundles.sh` with `${QMT_VERSION#v}`, then `collect-bundles.sh`, and uploads `qmt-desktop-<target>`. Linux legs install `dpkg-dev`.
  - Tick this task once a `workflow_dispatch` nightly passes `desktop-package` on all five targets (9.1).
- [x] 3.2 Check that `collect-bundles.sh` still collects the renamed bundles (`QueryMT Desktop.app.tar.gz`, the dmg) and update its fixtures in `test-release-scripts.sh`.
  - Verify: `scripts/release/test-release-scripts.sh` passes.

  Recorded: no change to `collect-bundles.sh` was needed (its `*.app.tar.gz` and `*.dmg` globs match `QueryMT Desktop.app.tar.gz`, space included). The macOS fixture now uses the new bundle name, and `scripts/release/test-release-scripts.sh` passes. Also run against the real macOS output of the 2.1 stand-in run: it collected `QueryMT-Desktop-nightly-macos-aarch64.app.tar.gz` and `.dmg`.

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

- [x] 5.1 Create the minisign key pair (no password for the CI key):
  - Commit the public key as `crates/querymt-gpui/packaging/update-key.pub`.
  - Add the secret key to the repository secret `QMT_UPDATE_SIGNING_KEY`.
  - Store an offline backup.
  - Verify: `minisign -Vm <file> -p crates/querymt-gpui/packaging/update-key.pub` verifies a test file signed with the key. The user confirms the secret exists (needs repository admin; ask the user).

  Status (done):
  - The public key (`5B82BABDB3F9E327`) is committed as `crates/querymt-gpui/packaging/update-key.pub`.
  - The user reports that repository secret `QMT_UPDATE_SIGNING_KEY` is set (not checkable from here).
  - Verified by the user: `minisign -Vm <test file> -p crates/querymt-gpui/packaging/update-key.pub` printed "Signature and comment signature verified" and `Trusted comment: version:0.0.0 channel:nightly`. The committed public key matches the secret key the user holds.
  - The user confirmed that an offline backup of the secret key exists.
- [x] 5.2 `scripts/release/write-release-json.sh`: named flags `--desktop-dir` and `--version-name` (D7).
  - Writes `desktop_updates` with file, size and SHA-256 for the five platform keys, and fails when any file is missing.
  - The existing fields stay unchanged.
  - Coordinate the flag names with `add-agent-session-core` if it has landed.
  - Covers:
    - desktop-self-update / Manifest lists every self-updatable platform
    - desktop-self-update / Manifest keeps the existing fields
    - desktop-self-update / Missing asset or key fails the release (missing-asset half)
  - Tests in `scripts/release/test-release-scripts.sh`: `manifest lists every self-updatable platform`, `manifest keeps the existing fields`, `missing desktop asset fails`.
  - Verify: `scripts/release/test-release-scripts.sh` passes.

  Recorded: passes. The four positional arguments are unchanged and the two flags are optional but go together (`desktop flags go together` checks the error). The `add-agent-session-core` change has not landed in this tree (the script had only the four positional arguments), so there was nothing to merge; whichever change lands second extends the flags (`--session-core-archive`). The file names follow `collect-bundles.sh` (`QueryMT-Desktop-<name>-<suffix>`); the dmg and deb in the directory are not listed. `nightly.yml` and `release.yml` pass `--desktop-dir artifacts --version-name <asset label>` (`nightly`, or the tag).
- [x] 5.3 Add `scripts/release/sign-release-json.sh <release.json> <version> <channel>`. It reads the key from `QMT_UPDATE_SIGNING_KEY`, writes `release.json.minisig` with trusted comment `version:<v> channel:<c>`, and fails without the key.
  - Add a step in `nightly.yml` and `release.yml` after "Write release.json" that installs `minisign` and runs it.
  - Covers:
    - desktop-self-update / Signature covers the manifest
    - desktop-self-update / Missing asset or key fails the release (key half)
  - Tests in the release-script tests: `signature covers the manifest` (a throwaway key generated in the test, verified with `minisign -V` and the trusted comment checked), `missing signing key fails`.
  - Verify: `scripts/release/test-release-scripts.sh` passes; `actionlint` is clean.

  Recorded: `scripts/release/test-release-scripts.sh` passes (all `ok`, minisign 0.12; throwaway keys generated in the test). `actionlint` on `nightly.yml` and `release.yml` is clean. `shellcheck -x -P SCRIPTDIR -S warning` is clean on the scripts this change adds or edits; its only warning is an existing SC2206 in `scripts/release/stable-gate.sh`, which this change does not touch.
  - Tests: `signature covers the manifest` (verifies with `minisign -V` and checks `Trusted comment: version:<v> channel:<c>`; a changed manifest does not verify), `missing signing key fails` (non-zero, no `.minisig` written).
  - Added beyond the task: the script verifies its own signature against the committed public key (`QMT_UPDATE_PUBLIC_KEY` overrides it in tests), so a secret that is not the pair of the key embedded in the app fails the release instead of shipping updates no app accepts. Test: `signing with a key that is not the embedded key's pair fails`.
  - Workflows: `nightly.yml` and `release.yml` install minisign and run the script after "Write release.json" (before the draft upload); both also install minisign before "Test release scripts", because those tests now sign a throwaway manifest. `release.json.minisig` is published with the other assets (`artifacts/*`).

## 6. Updater core (pure logic)

- [x] 6.1 Add dependencies (design D13): `ureq` 3 (rustls, no default features), `minisign-verify`, `semver`, `sha2`, and `tar` + `flate2` (macOS), all under native target tables.
  - Verify:
    - `cargo tree -p querymt-gpui -e normal -i aws-lc-rs` reports nothing;
    - the wasm check still passes;
    - `Cargo.lock` only gains `minisign-verify` and new edges.

  Recorded:
  - `cargo tree -p querymt-gpui -e normal -i aws-lc-rs`: "did not match any packages" (aws-lc-rs is not in the graph; `ring` is, through rustls).
  - wasm check passes (only the existing `tip_with_key` warning).
  - `Cargo.lock` gains the package `minisign-verify` 0.3.0 and edges on `querymt-gpui` (`minisign-verify`, `semver`, `tar`, `flate2`, plus the dev-dependencies `ed25519-dalek` 2.2.0 and `blake2`, already locked). No other package is added.
  - Added beyond the task: dev-dependencies `ed25519-dalek` 2 and `blake2` 0.10. The tests sign fixtures with a throwaway key in the format `minisign -S` writes, because `minisign-verify` only verifies.
  - `ureq` and `tar`/`flate2` have no user in the app yet (the fetch and macOS install code is group 7), so cargo prints two `unused dependency` manifest warnings on `cargo cl-gpui`. They are warnings only (the command exits 0). `ureq` is used since 7.1; `tar`/`flate2` (macOS) stay unused until 8.2, so `cargo cl-gpui` prints one such warning until then.

- [x] 6.2 `build.rs` emits `QMT_UPDATE_CHANNEL` (D6). `src/update/mod.rs` has the channel from the build and from preferences.
  - Covers: desktop-self-update / Nightly build follows the nightly channel, and the channel half of Development build makes no update request.
  - Tests: `nightly_build_follows_the_nightly_channel`, plus a build-channel table test `build_channel_from_version` (dev, nightly, stable).
  - Verify: `cargo t-gpui -E 'test(/nightly_build_follows|build_channel_from_version/)'` passes.

  Recorded: 3 tests pass. `build.rs` includes `src/update/channel.rs` with `#[path]` and the app reads the same function through `update::build_channel_from_version`, so the table cannot drift between build and app. `development_build_makes_no_update_request` (channel half) also passes. `Channel` and `UpdatePrefs` live in `update/prefs.rs`, which is in the web build too because `Prefs` has `updates`; the rest of `update/` is native only.

- [x] 6.3 `src/update/verify.rs`: verify the manifest against the embedded key and trusted comment (D7), and check the asset's size and SHA-256. Fixtures are signed in the test with a throwaway key; the production key is a `const` that tests can replace.
  - Covers:
    - desktop-self-update / Bad signature is refused
    - desktop-self-update / Signature for another version is refused
    - desktop-self-update / Corrupted download is rejected (verification half)
    - desktop-self-update / Nightly replaced during download (verification half)
  - Tests: `bad_signature_is_refused`, `signature_for_another_version_is_refused`, `corrupted_download_is_rejected`, `nightly_replaced_during_download`.
  - Verify: `cargo t-gpui -E 'test(/bad_signature_is_refused|signature_for_another_version|corrupted_download_is_rejected|nightly_replaced_during_download/)'` passes.

  Recorded: 4 named tests pass, plus `a_correctly_signed_manifest_is_accepted`, `the_committed_key_refuses_a_signature_from_another_key`, `a_manifest_for_another_channel_is_refused`, `a_signed_manifest_that_is_not_json_is_refused`, `the_listed_asset_is_accepted` and `embedded_key_is_a_valid_minisign_key` (the committed `update-key.pub` decodes). `verify_manifest` takes the key as a parameter, and `verify_release` passes the embedded one. The manifest JSON is parsed only after the signature verified. `AssetCheck` hashes a download as it is read, so installers are not held in memory.

- [x] 6.4 `evaluate`, `is_suppressed` and the channel check.
  - Covers:
    - desktop-self-update / Newer signed version is offered
    - desktop-self-update / Same or older version is not offered
    - desktop-self-update / Switching to stable never downgrades
    - desktop-self-update / Skipped version stays quiet
    - desktop-self-update / Manual check shows skipped version
  - Tests: `newer_signed_version_is_offered`, `same_or_older_version_is_not_offered`, `switching_to_stable_never_downgrades`, `skipped_version_stays_quiet`, `manual_check_shows_skipped_version`.
  - Verify: `cargo t-gpui -E 'test(/newer_signed_version|same_or_older_version|switching_to_stable|skipped_version_stays_quiet|manual_check_shows_skipped/)'` passes.

  Recorded: 5 named tests pass, plus `a_version_that_is_not_semver_is_an_error`. The channel check is `verify::check_channel`, used by `verify_manifest` (`a_manifest_for_another_channel_is_refused`).

- [x] 6.5 `detect_install_kind` (D9).
  - Covers (the kind half; the UI half is in 7.3):
    - desktop-self-update / Deb install is notify-only
    - desktop-self-update / Translocated macOS app is notify-only
  - Tests: `deb_install_is_notify_only`, `translocated_macos_app_is_notify_only`, plus `install_kind_table` (writable `.app`, NSIS location match, AppImage writable and read-only, Nix store).
  - Verify: `cargo t-gpui -E 'test(/deb_install_is_notify_only|translocated_macos_app|install_kind_table/)'` passes.

  Recorded: 3 tests pass, plus `install_parent_is_the_directory_that_needs_write_access`. The probe holds the facts (the exe, `$APPIMAGE`, the NSIS `InstallLocation`, whether the parent directory is writable) and paths are compared as text, so the Windows and macOS rules run on any host. Windows matching ignores case, separator style and a trailing separator. `NotifyOnly` carries a reason (`Translocated`, `ReadOnly`, `Unmanaged`) for the UI in 7.3.

- [x] 6.6 Scheduling (`check_due`, single check in flight) and `UpdatePrefs` with serde defaults.
  - Covers:
    - desktop-self-update / First automatic check after start
    - desktop-self-update / Automatic checks off
    - desktop-self-update / Concurrent check is ignored
    - desktop-self-update / Older state loads with update defaults
  - Tests: `first_automatic_check_after_start`, `automatic_checks_off`, `concurrent_check_is_ignored` (drive `check_due` with an injected clock), `older_state_loads_with_update_defaults`.
  - Verify: `cargo t-gpui -E 'test(/first_automatic_check|automatic_checks_off|concurrent_check_is_ignored|older_state_loads_with_update_defaults/)'` passes.

  Recorded: 4 named tests pass, plus `a_manual_check_delays_the_next_automatic_one`. `Scheduler` has no timers: `auto_due(auto_check, since_start)` and `start(trigger, ..)` take the clock as an argument. `UpdatePrefs` is in `Prefs.updates`. `last_check` is a Unix timestamp for display; the scheduler's own clock is time since start.

- [x] 6.7 Run the group checks and record the commands and counts here:
  - full `cargo t-gpui`;
  - fmt;
  - `cargo cl-gpui`;
  - the wasm check.

  Recorded (macOS arm64):
  - `cargo t-gpui`: 643 run, 643 passed, 5 skipped (28 of them are the new `update::` tests).
  - `cargo fmt -p querymt-gpui -- --check`: clean.
  - `cargo cl-gpui`: exit 0, with only the two `unused dependency` manifest warnings described in 6.1.
  - wasm check: passes, only the existing `tip_with_key` warning.
  - `update/mod.rs` has `#![allow(dead_code)]` because nothing calls these functions until the store does (group 7). It stays until 8.5 (the update code that uses downloads and installers); see 7.2.

## 7. Updater runtime: fetch, store and UI

- [x] 7.1 `src/update/fetch.rs`: a ureq agent with timeouts and proxy settings from the environment, fetching the manifest and its signature. Downloads run on a dedicated thread and report progress over a channel. The base URL is injectable for tests.
  - Covers (fetch half):
    - desktop-self-update / Automatic check offline
    - desktop-self-update / Manual check offline
  - Tests against a local `std::net::TcpListener` HTTP stub:
    - `automatic_check_offline` and `manual_check_offline`: an unreachable port gives a network failure, and the state reaches a log-only error (automatic) or a shown error (manual);
    - `download_reports_progress`.
  - Verify: `cargo t-gpui -E 'test(/automatic_check_offline|manual_check_offline|download_reports_progress/)'` passes.

  Recorded: the 3 named tests pass (`automatic_check_offline` and `manual_check_offline` live in `update/check.rs` because they test `after_check`, the function that turns a failed check into a log-only or shown error; `download_reports_progress` is in `fetch.rs`). The stub is `update/test_stub.rs`: a `TcpListener` HTTP server that counts requests and can change a response between requests. Also tested against it: corrupted, short and long downloads delete the file; a missing asset leaves no file; a check requests only the channel's manifest and signature; skipped versions; a bad signature. Proxy settings come from the environment: ureq 3's default (`Proxy::try_from_env`). The feed's base URL is a `Feed` value, and `https_only` is on unless the base is plain `http` (the stub).

- [x] 7.2 `src/store/updates.rs`: `UpdateState`, a scheduler started from `AgentsStore::new` (none for `dev`), `check_updates`, `skip_version`, and persisted preferences (D8). `install_update` and `restart_to_update` move to 8.5 and 8.4: they need the per-kind installers.
  - Covers:
    - desktop-self-update / Development build makes no update request
    - desktop-self-update / No download without consent
    - desktop-self-update / Preferences survive restart
  - Tests (GPUI test context with the HTTP stub; the request counter is read from the stub):
    - `development_build_makes_no_update_request`;
    - `no_download_without_consent` (after a check finds an update, the stub saw no asset request);
    - `preferences_survive_restart` (save, reload `Persisted`).
  - Verify: `cargo t-gpui -E 'test(/development_build_makes_no_update_request|no_download_without_consent|preferences_survive_restart/)'` passes.

  Recorded: the 3 named tests pass, plus `the_schedule_checks_after_the_first_delay`, `automatic_checks_off_make_no_request`, `concurrent_check_is_ignored_by_the_store`, `skipping_a_version_keeps_automatic_checks_quiet`, `a_failed_automatic_check_is_quiet_and_a_manual_one_is_shown` and `switching_channel_clears_the_offer_and_checks`.
  - `Scheduler::wait_until_due` drives the loop: it sleeps until the next check is due, at most 60 s, so turning the switch on is noticed. The test clock is a `skew` added to the time since start, because GPUI's test executor does not move `Instant`.
  - Moved to group 8 (a plan change, flagged for review): `install_update` and `restart_to_update`. Both need the installers, and a download with nothing to install it would be a button that fails; the state variants `Downloading`, `Installing` and `ReadyToRestart` and the download code (`fetch::spawn_download`) are written and tested already. `update/mod.rs` keeps `#![allow(dead_code)]` until 8.5 uses them.
  - Switching channel clears the offer (it belonged to the other channel) and starts a manual check, so the user sees the result.

- [x] 7.3 Read the gpui-kit-design-guides skill. Then add the Updates card in `src/ui/settings.rs` `render_settings` and the sidebar "Update available" item (D12).
  - Covers (UI: version, release link and the move hint, no install action):
    - desktop-self-update / Deb install is notify-only
    - desktop-self-update / Translocated macOS app is notify-only
  - Tests: `notify_only_shows_release_link` and `translocated_shows_move_hint`, GPUI view tests using the debug selectors the settings tests already use.
  - Verify: `cargo t-gpui -E 'test(/notify_only_shows_release_link|translocated_shows_move_hint/)'` passes; screenshot review against the design guides.

  Recorded:
  - Read the whole guide (the sections up to "Interface language" in full; the rest are checklists).
  - Settings > General has the Updates card (`ui/updates.rs`): update channel (segment bar), "Check automatically", the status row with "Check now", and, when a newer version exists, one row with the version, a "Release page" link and "Skip this version". Builds that cannot update show one line and no controls. The sidebar and the collapsed rail show "Update available" (or "Restart to update") above Settings; it opens the card.
  - `notify_only_shows_release_link` and `translocated_shows_move_hint` pass, plus `development_build_shows_updates_unavailable`, `the_card_controls_update_the_preferences` and `the_sidebar_item_opens_the_updates_card`.
  - Screenshot review (user's screenshot of Settings > General, dark theme, collapsed sidebar, release build 0.6.0): the card matches its neighbours (row rhythm, flat "Check now" like "Run setup again", visible selected segment, Switch for the immediate setting). Two copy findings, fixed: the first row was titled "Version" while its control picks the channel, and its detail repeated the selected segment (now "Update channel" / "QueryMT Desktop 0.6.0"); the idle status row was titled "Updates", restating the card (now "Check for updates"). Tests pass after the change.
  - Not yet seen on screen: the offer row, the move hint and the sidebar item (they need a found update). The offer row gains the "Install update" action in 8.5, so its screenshot review happens there, with the final row.
  - Fixed pixel sizes follow `settings.rs`'s own helpers, so zoom behaviour is the page's existing one.
## 8. Updater runtime: install and relaunch

- [x] 8.1 Stop local process agents before replacing files: a store method that disconnects every `Endpoint::Process` agent and waits for the child to exit, with a timeout.
  - Covers: desktop-self-update / Local agents stop before files change.
  - Test: `local_agents_stop_before_files_change`. A fake agent script records its exit time and the install step records its first rename; assert the order.
  - Verify: `cargo t-gpui -E 'test(/local_agents_stop_before_files_change/)'` passes (Unix).
  - Done:
    - `transport::Exit` (a flag plus a condvar) is set by the thread that owns the child, after `kill` and `wait`. `Connection.exit` carries it; the store keeps it in `AgentRuntime.exit`.
    - `AgentsStore::stop_local_agents(timeout, cx)` (`store/updates.rs`) disconnects each process agent and returns a task that resolves to the ids still running after the timeout. The wait runs on a background thread. The install must abort when the list is not empty (decided with the user).
    - The test runs a real `sleep` child, stops it, and checks that the pid is gone before the stand-in first rename. It does not use a recorded exit time: `kill` cannot be trapped.
    - Extra tests: `an_agent_that_does_not_stop_is_reported`, `exit_wait_times_out_until_marked`, `closing_the_channel_stops_the_process_and_marks_it_exited`.
    - An agent still starting (spawned but not yet recorded) has no signal to wait on; its child is killed by the connection's own teardown. Noted in the doc comment.
    - `stop_local_agents` carries a temporary `#[allow(dead_code)]`; 8.5 removes it.
- [x] 8.2 macOS install through the `self_update` crate (D10): `src/update/install/macos.rs` becomes a thin wrapper that builds a `self_update::backends::custom::Update` and keeps our two checks as hooks.
  - Reopened. The first draft was a hand-written installer (unpack, rename with rollback; 9 tests). After the spike recorded in D10, the decision (with the user) is to rely on the maintained crate for the macOS swap and delete that code. Windows and AppImage stay ours.
  - Covers:
    - desktop-self-update / macOS bundle is replaced in place
    - desktop-self-update / Failed install keeps the old version (macOS)
    - desktop-self-update / Corrupted download is rejected (cleanup half)
    - desktop-self-update / Local agents stop before files change (the hook half)
  - Wrapper (D10): the source is one `Release` from the verified `Offer`, with the SHA-256 as the asset digest; `current_version("0.0.0")`, `target(<asset file name>)`, bundle mode, no prompt, no output. `progress_callback` sends `Progress`. `verify_archive` runs `AssetCheck` (size and SHA-256) and the archive scan (absolute or `..` entries, entries outside the bundle, hard links, outward links; `tar` + `flate2`). `verify_binary` runs `codesign --verify --strict` and then the stop-agents callback, failing when an agent is still running. Both `codesign` and the stop-agents step are arguments, so tests inject them.
  - Add `self_update` (macOS only, features in D13) to `Cargo.toml`; check `Cargo.lock` gains only `self_update`, `self-replace`, `zipsign-api`, `utf_types` and edges; check `aws-lc-rs` still is not in the querymt-gpui graph; run `cargo hakari verify` if installed.
  - Tests on the local stub server (`update/test_stub.rs`) with a fixture tarball and a temp `Applications` directory (the real crate runs; nothing is mocked except `codesign` and the agent stop):
    - `macos_bundle_is_replaced_in_place` (also: no leftovers next to the bundle, progress was reported);
    - `failed_install_keeps_the_old_version`: the stop-agents callback fails, and separately `codesign` fails; the old version is intact and nothing is left behind;
    - `corrupted_download_leaves_no_files`: wrong digest, wrong size, truncated gzip, an HTML page, a bundle without its executable;
    - `unsafe_archive_entries_are_rejected` (traversal, absolute, outward symlink, hard link).
  - The crate's own rollback when the last rename fails is not tested by us (it cannot be provoked from outside); it is the crate's.
  - Verify: `cargo t-gpui -E 'test(/macos_bundle_is_replaced_in_place|failed_install_keeps_the_old_version|corrupted_download_leaves_no_files|unsafe_archive_entries_are_rejected/)'` passes.
  - Done:
    - `install(bundle, version, url, asset, timeout, progress, steps)` in `update/install/macos.rs` builds the `custom::Update` as in D10 and maps the crate's errors to `InstallError` text for the card. `Steps` injects `codesign` and `stop_agents`. The hand-written unpack and rename code and its tests are deleted; the entry and link scan (`scan_archive`) stays, as the `verify_archive` hook.
    - Two decisions beyond the first plan, now in D10: the crate gets no digest (our `AssetCheck` in `verify_archive` checks size and SHA-256 once, so the card shows one set of messages), and our `verify_binary` hook checks for `Info.plist` and a runnable executable, because the crate compares the executable path only when the updater runs from inside the bundle (the test process does not).
    - Tests (5, real crate against the local stub server, temp `Applications`): the four named ones, plus `the_real_signature_check_tells_signed_from_unsigned` (real `codesign` on an ad-hoc signed temp bundle: signed passes, unsigned and tampered fail). `macos_bundle_is_replaced_in_place` also asserts that `codesign` and the agent stop both ran while the old version was still installed, that nothing is left next to the bundle and that progress reaches the total. `corrupted_download_leaves_no_files` covers wrong SHA-256, wrong size, truncated gzip, an HTML page, no executable and an executable without the x bit, and asserts the agents were not stopped for a bad download.
    - Verify run: the 4 named tests pass (4 of 4); the whole `update::install` group passes (20 of 20).
    - Dependencies: `Cargo.lock` gains `self_update` 1.3.3, `self-replace` 1.5.1 and `zipsign-api` 0.2.2 (plus edges). `reqwest` is not compiled (`cargo tree -p querymt-gpui -i reqwest@0.13.5` finds nothing); its lock entry and an `encoding_rs` edge come from weak features that are off. `aws-lc-rs` is still not in the graph. `cargo-hakari` is not installed here: `crates/querymt-workspace-hack` and `crates/agent*` show no diff and querymt-gpui is excluded from hakari; `cargo hakari verify` stays in 9.4. (My earlier plan text listed `utf_types`; that was an artifact of a scratch crate's fresh resolution and is not in this lock.)
    - Not tested by us: the crate's rollback when the last rename fails. A failure cannot be provoked from outside, and a read-only parent trips the permission check first. It is the crate's code (read in the spike, `swap_bundle`).
    - Spike (recorded in D10), run from `/tmp/su-spike` before the change: swap from inside the installed bundle, wrong digest, truncated gzip, HTML page and a failing hook all left the old version and no leftovers; 769 progress calls for 6 MB.
- [x] 8.3 `src/update/install/appimage.rs` (D10): sibling temp file, chmod, rename over `$APPIMAGE`.
  - Covers: desktop-self-update / AppImage is replaced in place.
  - Test: `appimage_is_replaced_in_place`, on temp files, asserting the path's content and mode 0755.
  - Verify: `cargo t-gpui -E 'test(/appimage_is_replaced_in_place/)'` passes.
  - Done:
    - Ours, not the crate's (D10: a raw asset is truncated by the crate when `bin_path_in_archive` equals the asset name, and the file loses its executable bit). `staging_path` (hidden file next to `$APPIMAGE`), `prepare` (checks the ELF magic, `chmod 755`; the download is deleted on any error or when the install is dropped), `Prepared::commit` (one `rename` over `$APPIMAGE`, injectable through `Hooks`).
    - Tests (4): `appimage_is_replaced_in_place` (content and mode 0755, nothing left), `a_failed_rename_keeps_the_old_appimage_and_cleans_up`, `a_download_that_is_not_an_appimage_is_refused_and_removed`, `an_abandoned_install_deletes_its_download`. They run on Unix, so on this Mac too.
    - The download itself is `fetch::spawn_download` to `staging_path`, started by `install_update` (8.5).
- [x] 8.4 `src/update/install/windows.rs` (D11): download the setup to `%TEMP%` and spawn it detached with `/S /UPDATE /D=<dir>`. Also the restart path, `restart_to_update`: stop agents, drop the instance listener, spawn the new app, quit.
  - Covers:
    - desktop-self-update / Windows installer runs after the app exits
    - desktop-self-update / Restart opens the new version
  - Tests:
    - `windows_installer_runs_after_the_app_exits`: builds the argument list, `/D` last and unquoted (pure test).
    - `restart_opens_the_new_version`: asserts the order "listener dropped, then child spawned, then quit" through injected effects, and that a second `forward` after the drop reaches nobody (reuses `quick::instance` test helpers).
  - Verify: `cargo t-gpui -E 'test(/windows_installer_runs_after|restart_opens_the_new_version/)'` passes. The real Windows run is in 9.2.
  - Done:
    - `install/windows.rs`: `setup_path` (`%TEMP%\QueryMT-Desktop-update-<version>.exe`, version made safe for a file name), `launch` (`/S /UPDATE`, and `/D=<dir>` as the raw tail, never quoted), `check_setup` (refuses and deletes a download that is not a Windows executable). `install/mod.rs`: `Launch`, `launch_for` (macOS `open -n <bundle>`, AppImage path) and `spawn_detached`.
    - `install/restart.rs`: `Effects` and `relaunch` (release the listener, start the new version, quit; if the start fails, listen again and report, no quit). `AppEffects` is the real one.
    - `Listening::release` (`quick/instance.rs`) signals the listener thread and waits, bounded, until the socket name is free; a plain drop only signals, so switching the quick panel off does not wait. `QuickHost::release_instance_socket` and `restore_instance_socket` use it.
    - Tests: `windows_installer_runs_after_the_app_exits` (argument list, spaces in the directory, `/D` last, unquoted, raw), `the_setup_is_saved_under_a_safe_name`, `only_a_windows_executable_may_be_run`; `restart_opens_the_new_version` (order "listener dropped, child spawned, quit", and a `forward` right after reaches nobody), `a_start_that_fails_keeps_the_app_running_and_listening`; `release_frees_the_name_before_it_returns` (20 rounds, no sleep) and `the_new_version_is_started_the_way_its_install_kind_needs`.
    - Verify run: `restart_opens_the_new_version` and `windows_installer_runs_after_the_app_exits` pass; the six `quick::instance` and `restart` tests pass.
    - The Windows branch of `spawn_detached` (`raw_arg`, `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`) is `#[cfg(windows)]` and was not compiled by these runs. I type-checked the same code on its own with `rustc --target x86_64-pc-windows-msvc --emit metadata`, which passes. The whole crate cannot be checked for Windows here (`ring`'s build script needs a C cross-compiler), so the real run in 9.2 stays open.
    - `spawn_detached`, `windows.rs`, `release_instance_socket` and `restore_instance_socket` have no caller until 8.5, so they rely on the update module's `allow(dead_code)`; `cargo cl-gpui` reports the two `QuickHost` methods until then.
- [x] 8.5 `install_update` (store, `src/store/updates.rs`) and the card's actions (moved from 7.2 and 7.3).
  - `install_update`: only after the user asks (the button); stops local agents (8.1); downloads with `fetch::spawn_download` to the kind's staging path (macOS and AppImage: next to the installed file; Windows: `%TEMP%`), reporting `Downloading` progress; installs with the kind's installer (8.2–8.4); ends in `ReadyToRestart` (macOS, AppImage) or quits into the setup (Windows); any failure ends in `Failed` with the old version intact.
  - Screenshot review of the offer row, the translocated move hint and the sidebar item against the design guides (deferred from 7.3).
  - Card: an "Install update" button for installable kinds (the default commit of the row, so accent), "Restart now" and "Later" after an install; the progress shown in the status row.
  - Covers: desktop-self-update / No download without consent (the button half), Failed install keeps the old version.
  - Tests: `install_update_downloads_and_installs_on_request` (stub asset, temp install target), `a_failed_download_changes_nothing`, `the_install_button_is_not_shown_for_notify_only_installs`.
  - Verify: `cargo t-gpui -E 'test(/install_update_downloads|a_failed_download_changes_nothing|install_button_is_not_shown/)'` passes. Then remove `#![allow(dead_code)]` from `update/mod.rs`.
  - Status: done, with two decisions recorded (plan change, agreed with the user):
    - Screenshot review: done on the user's screenshot of the idle card (Settings > General). Two wording fixes came out of it and are applied: the channel row now describes the control ("Stable gets releases. Nightly gets a build every night.") instead of showing the app version, and the idle status row is titled "Updates" (it repeated its "Check now" button as "Check for updates"); the version moved into the status detail ("QueryMT Desktop 0.6.0. Not checked yet."). The states that need an offer (offer row with and without the install button, translocated move hint, "Restart now" row, sidebar item) cannot be shown by a local build, because the feed URL is compiled in. They are reviewed in 9.3 on real machines.
    - No separate "Later" button (decided with the user). The row has "Restart now", and its text says the new version starts the next time the app is opened, so later is simply not clicking. A button that does nothing is a dead control. The spec scenario "offer restart now or later" is covered by that text.
    - Re-run after the wording change: `cargo t-gpui` 698 run, 698 passed, 5 skipped; fmt clean; `cargo cl-gpui` clean; wasm check passes.
  - Done:
    - `install_update` (`store/updates.rs`), only from the card's button: the state goes to `Downloading` at once, then per install kind (`run_install`): AppImage = `fetch::spawn_download` to the staging path, `appimage::prepare`, stop agents, `commit`; macOS = `macos::install` on its own thread, which asks the UI thread to stop the agents between checking the bundle and swapping it; Windows = download the setup to `%TEMP%`, `check_setup`, stop agents, `relaunch` with `windows::launch`. Any error ends in `Failed` and the installed version is untouched; the agents the install stopped are connected again (also after a successful macOS or AppImage install, so "later" is usable).
    - `restart_to_update` (the "Restart now" action): stops the agents, then `restart::relaunch` with the real effects (`AppEffects` releases the quick panel's socket through `QuickHost`, starts the new version detached, quits). If the new version cannot be started the app stays and says so.
    - `OfferAction::Install { closes_app }`: the offer row's accent button reads "Install update", or "Install and restart" on Windows, where the setup needs the app out of the way, with a line saying so. Notify-only installs have no install button. In the status row "Restart now" replaces "Check now" once installed, and "Check now" is disabled while a download or install runs.
    - `#![allow(dead_code)]` is gone from `update/mod.rs`. What it hid: `Listening`/`QuickHost` methods, `verify_release`, `VerifyError::is_untrusted_release` (both deleted, nobody called them; their one test now calls `verify_manifest`), `verify_asset` and `build_channel_from_version` (now test-only), `channel::name_for_version` (the module is test-only in the app; `build.rs` includes the file by path), `parse_reg_install_location` (allowed off Windows, with a reason) and `Channel::as_str` (allowed on wasm).
    - Tests, store: `install_update_downloads_and_installs_on_request` (also: nothing downloaded before the click, one download, mode 0755, nothing left), `a_failed_download_changes_nothing` (a replaced asset, and a 404), `a_failed_install_reconnects_the_agents_it_stopped` (agent stopped before the failing last step, connected again), `a_setup_that_cannot_start_is_removed_and_reported` (the Windows arm up to the start, on a Unix host), `install_update_replaces_a_signed_mac_bundle` (macOS only: real download, real crate, real `codesign` on an ad-hoc signed bundle, a running agent stopped and connected again). UI: `the_install_button_is_not_shown_for_notify_only_installs`, `an_installable_offer_has_an_install_button` (its click on an offer without an installer for the platform fails visibly instead of downloading), `ready_to_restart_offers_restart_now`. The tests that use real threads call `allow_parking()`.
    - Verify run: the three named tests pass (3 of 3).
    - Not run: a click on "Restart now" (it quits the app); the Windows setup actually running (9.2).
- [x] 8.6 Run the group checks and record the commands and counts here:
  - full `cargo t-gpui`;
  - fmt;
  - `cargo cl-gpui`;
  - the wasm check.
  - Recorded (macOS arm64, after 8.5's code; run again if the screenshot review changes the UI):
    - After the CodeRabbit fixes below: `cargo t-gpui` 702 run, 702 passed, 5 skipped; fmt, `cargo cl-gpui`, the wasm check, `scripts/release/test-release-scripts.sh` and `openspec validate --strict` all pass; `makensis` compiles `installer.nsi` (with qmtcode).
    - CodeRabbit review of the uncommitted files (7 findings, validated against the code): fixed (1) update results that outlived a channel switch or "Reset app data" (`Updates.generation`, `invalidate`, a `recheck` after a switch during a running check, and `install_update` refusing an offer of another channel; tests `a_check_that_ends_after_a_channel_switch_is_dropped`, `resetting_app_data_drops_the_offer_and_a_running_check`, `an_offer_of_another_channel_is_not_installed`); (2) the NSIS migration kept deleting the old uninstaller and registry key when the old uninstaller failed (now kept, install goes on); (3) the `updates` field in two test fixtures lacked the wasm cfg. Not problems: the `v1.2.3` channel input (already handled and tested), the lockfile and hakari (`querymt-gpui` is excluded; `cargo metadata --locked` passes), `mod update` on wasm (gated, wasm check passes), and an explicit qmtcode path being replaced by the bundled copy (it is not; test `an_explicit_path_is_used_even_when_a_copy_is_bundled` added). The review did not cover untracked files.
    - `cargo t-gpui` before that: 698 run, 698 passed, 5 skipped.
    - `cargo fmt -p querymt-gpui -- --check`: clean.
    - `cargo cl-gpui`: exit 0, no warnings (the earlier "unused dependency `tar`" warning is gone, `tar` and `flate2` are used by the archive scan).
    - `RUSTC_BOOTSTRAP=1 cargo check -p querymt-gpui --target wasm32-unknown-unknown`: passes; the only warning is the existing `tip_with_key`.
    - `scripts/release/test-release-scripts.sh`: all passed. `openspec validate bundle-qmtcode-and-self-update --strict`: valid.
    - `git diff --stat -- crates/agent crates/agent-* crates/querymt-workspace-hack`: no changes.
    - Not run here: the Windows-only `no_console_window_for_a_local_agent`, and any Windows or Linux build of the crate (`ring` needs a C cross-compiler); `cargo hakari verify` (not installed; 9.4).

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

  Also review these on screen, against the design guides (carried over from 8.5): the offer row with "Install update" (and "Install and restart" on Windows), the translocated move hint (run the app from the mounted dmg or the Downloads folder), the "Restart now" row, and the sidebar "Update available" item.

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
