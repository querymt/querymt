# Proposal

## Why

QueryMT Desktop (crates/querymt-gpui) only works when the user has installed qmtcode separately and put it on PATH, and it has no way to update itself. The Tauri app it replaces bundles qmtcode and updates itself, so the GPUI app has to do both before the Tauri app can be retired.

## What Changes

- **Bundled qmtcode.** Every desktop installer ships the qmtcode built by the same release: inside the macOS `.app`, next to the exe in the Windows install, and in `/usr/lib/querymt-gpui/` for the deb and AppImage. Linux uses the static musl qmtcode. The Nix package gets qmtcode through a `lib/querymt-gpui/qmtcode` symlink.
- **Release pipeline.** A packaging job takes the compiled app and the qmtcode built by the same run. The app compile itself still runs alongside the qmtcode builds. CI fails if an installer does not contain qmtcode.
- **Local agent resolution.** A bare `qmtcode` command line uses the bundled copy first, then PATH. A command with a path is used as written. The agent log names the copy that started ("bundled" or "PATH", with its path).
- **Local agent seeding.** "Local qmtcode" is added when a bundled copy exists or qmtcode is on PATH. On Windows the PATH check now finds `qmtcode.exe`; before, it never matched. If the user deletes the seeded agent, it stays deleted. Reset app data restores it.
- **Agent processes on Windows** start without a console window.
- **App identity.** The app becomes "QueryMT Desktop":
  - macOS: `QueryMT Desktop.app`, bundle id `com.querymt.desktop`.
  - Windows: install folder and uninstall entry "QueryMT Desktop".
  - Linux: deb package `querymt-desktop`, desktop entry "QueryMT Desktop".

  This happens before the first self-updating release, because an in-place updater cannot follow a rename.
- **Signed release manifest.** `release.json` adds per-platform desktop update assets with size and SHA-256. Every release publishes `release.json.minisig`, a minisign signature whose trusted comment binds the version and channel. The fields existing readers use do not change.
- **Self-update.**
  - Packaged builds check the signed manifest of their channel (stable or nightly) 30 s after start and then every 6 hours. Users can also check by hand.
  - A newer version is offered and downloads only when the user asks. It installs only after the signature and SHA-256 both verify.
  - Self-install works for the macOS `.app`, Windows NSIS installs and AppImages. Deb, Nix, read-only and translocated installs are told about the new version and get a link instead.
  - Local agents stop before files are replaced, and the app relaunches into the new version.
  - Settings gains an Updates section: version, channel, automatic checks, Check now, skip this version.
  - Development builds never check.

## Non-goals

- Retiring the Tauri app and moving its users. That is the next change: a final Tauri release with a "moved" notice, removing the `querymt-desktop` nightly dispatch, and importing Tauri profiles. This change takes over the Tauri app's identity but does not import its data.
- Developer ID signing and notarization on macOS, and Authenticode on Windows.
- Download statistics or a query.mt redirect for update traffic.
- Delta updates, background (silent) installs, and updating qmtcode separately from the app.
- Updating the web build. It is served by qmtcode and changes when qmtcode does.
- LAN authentication for the dashboard (deferred to ACP v2).

## Capabilities

### New Capabilities

- `desktop-packaging`: what the QueryMT Desktop installers contain and are named (bundled qmtcode per platform, app identity, installer checks in CI).
- `desktop-local-agent`: how the desktop app finds and starts a local qmtcode (bundled vs PATH resolution, the seeded "Local qmtcode" agent and its dismissal, process start on Windows).
- `desktop-self-update`: the signed release manifest and the in-app update check, download, verification, install and relaunch, with update preferences.

### Modified Capabilities

None. No existing spec covers desktop packaging, local agent start or updates.

## Impact

- **Packages:**
  - `querymt-gpui`: native only. The update code is `cfg(not(target_family = "wasm"))` and wasm builds are unchanged.
  - `querymt-agent` and other Rust packages: unaffected.
- **New crate-local dependencies of querymt-gpui (native only):**
  - `ureq` 3 with rustls (already a build-dependency);
  - `minisign-verify`;
  - `semver`, `sha2`, `tar` and `flate2`, already in Cargo.lock.
- **CI and release:**
  - `.github/workflows/build.yml`: the desktop job splits into `desktop` (compile) and `desktop-package`.
  - `nightly.yml` and `release.yml`: sign `release.json`.
  - `scripts/release/write-release-json.sh` and its tests.
  - Packaging scripts under `crates/querymt-gpui/scripts/package/`, `packaging/windows/installer.nsi`, `packaging/nfpm.yaml`, and the `.desktop` file.
  - `flake.nix`.
  - New repository secret `QMT_UPDATE_SIGNING_KEY`, a minisign secret key. Its public key is committed in `crates/querymt-gpui/packaging/update-key.pub`.
- **`release.json` (wire shape):**
  - Adds the `desktop_updates` object. `version`, `channel`, `commit` and `published_at` keep their meaning.
  - The `add-agent-session-core` change also extends this script; the two edits must be merged.
- **Persisted state (`state.json`):** new `dismissed_seeds` and `updates` preferences with serde defaults, so existing files load unchanged. The state directory stays `querymt-gpui`.
- **Identity (BREAKING for installs):**
  - On macOS the app moves from `QueryMT.app` to `QueryMT Desktop.app`. Existing GPUI test installs must be removed by hand.
  - On Linux the deb package `querymt-gpui` is replaced by `querymt-desktop` (`Replaces`/`Conflicts: querymt-gpui`).
  - On Windows, existing installs are uninstalled by the new installer.
  - User state carries over on all platforms.
- **Installer size:** grows by the size of qmtcode.
- **Not affected:** public Rust API, agent config, hooks, middleware, ACP, provider wire shapes, the Extism contract, the session DB, Python bindings, typeshare types.
- **querymt-desktop (Tauri) repo:** no work in this change. Its retirement is the follow-up named in Non-goals.
