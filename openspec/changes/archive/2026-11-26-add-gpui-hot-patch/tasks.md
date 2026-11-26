# Tasks

This change sets `skip_specs: true` (opt-in developer tooling), so no task covers spec scenarios. Each task states its own verification. Commands run from the repository root. Each measurement is written under its task with the date, machine, `rustc -V`, `dx --version`, `RUSTC_WRAPPER`, the exact command and the result.

## 1. Tooling

- [x] 1.1 Install the Dioxus CLI 0.7.10:
  - `cargo binstall dioxus-cli@0.7.10`, or `cargo install dioxus-cli@0.7.10 --locked` if no binary is available;
  - then confirm that `~/.cargo/bin/dx --version` reports 0.7.10;
  - record `command -v dx`, to document the Deno `dx` clash.
  - Verify: both outputs are recorded below this task.
  - Record (2026-11-26, arm64 macOS, rustc 1.100.0, `RUSTC_WRAPPER=sccache`): `cargo binstall dioxus-cli@0.7.10 --no-confirm` installed `dx` from the GitHub release binary to `/Users/wiking/.cargo/bin/dx`. `~/.cargo/bin/dx --version` -> `dioxus 0.7.10 (57d6794)`. `command -v dx` -> `/opt/homebrew/bin/dx`, which is Deno (`dx --version` -> `deno 2.9.8`), so the clash is confirmed and the script must use the full path.
- [x] 1.2 Read `~/.cargo/bin/dx serve --help` and record:
  - the hot-patch flag spelling;
  - package and binary selection;
  - platform selection;
  - how arguments reach the app;
  - whether a cargo profile can be chosen.
  - Verify: the five answers are recorded below this task. Design D2 and D3 use them.
  - Record (`dx 0.7.10`, from `dx serve --help`):
    - flag: `--hot-patch` (a boolean flag, "experimental"). `--hotpatch` is not the 0.7.10 spelling. Also `--fat-binary` exists but is not needed.
    - package and binary: `-p/--package <PACKAGE>`, `--bin <BIN>`, `--example <EXAMPLE>`. `--features <FEATURES>` takes a space-separated list.
    - platform: `--desktop`, `--macos` or `--platform <desktop|macos|...>`. The script passes `--platform macos` per host OS (`--desktop` otherwise); `--web` is not used.
    - app arguments: `--args <ARGS>`, one string. There is no trailing `-- <args>` form in `dx serve`, so the script joins its arguments into one `--args` value (arguments with spaces are not supported).
    - profile: `--profile <PROFILE>` exists, so design D3's `dev-hot` can be tried in 3.3.
    - other: `--open` (browser) and `--always-on-top` (default true for desktop) are set for a native GPUI window: the script passes `--open false` and `--always-on-top false`. `-i/--interactive` is optional.

## 2. Feature and script

- [x] 2.1 Add the `hot-patch` feature and the optional, target-gated `gpui-pre = "=0.3.14"` dependency with `features = ["hot-reload"]` to `crates/querymt-gpui/Cargo.toml` (design D1), with a comment pointing to this change and to the gpui-kit pin.
  - Verify:
    - `cargo check -p querymt-gpui` passes;
    - `cargo check -p querymt-gpui --features hot-patch` passes;
    - `cargo tree -p querymt-gpui --features hot-patch -i subsecond` shows `subsecond` under `gpui-pre`;
    - `cargo tree -p querymt-gpui -i subsecond` (feature off) reports no match;
    - `cargo tree -p querymt-gpui --features hot-patch -d gpui-pre` lists one `gpui-pre` version.
  - Record (2026-11-26, arm64 macOS, rustc 1.100.0, `RUSTC_WRAPPER=sccache`): added `[features] hot-patch = ["dep:gpui-pre"]` and `gpui-pre = { version = "=0.3.14", optional = true, features = ["hot-reload"] }` under `[target.'cfg(not(target_family = "wasm"))'.dependencies]`.
    - `cargo check -p querymt-gpui`: passes. `cargo check -p querymt-gpui --features hot-patch`: passes.
    - `cargo tree -p querymt-gpui --features hot-patch -i subsecond`: `subsecond v0.7.10` <- `dioxus-core` <- `dioxus-devtools-types` <- `gpui-pre v0.3.14` <- `gpui-base`/`gpui-component`/`gpui-kit` <- `querymt-gpui`.
    - `cargo tree -p querymt-gpui -i subsecond` (feature off): `error: package ID specification 'subsecond' did not match any packages`, i.e. no match.
    - `cargo tree -p querymt-gpui --features hot-patch -d`: every `gpui-pre` line is `v0.3.14`; no second version.
- [x] 2.2 Check that default and special builds are unaffected:
  - `cargo hakari generate --diff` (or the `lint.yml` workspace-hack check command) reports no change;
  - `cargo check -p querymt-gpui --target wasm32-unknown-unknown --features hot-patch` builds without `subsecond`; if the wasm target is not installed, record it.
  - Verify: both results are recorded. The `Cargo.lock` diff adds only the hot-reload dependency closure; list the added package names below this task.
  - Record:
    - `cargo hakari generate --diff`: no output, exit 0. `cargo hakari manage-deps --dry-run`: `no operations to perform`. `cargo hakari verify`: `querymt-workspace-hack works correctly`.
    - `cargo check -p querymt-gpui --target wasm32-unknown-unknown --features hot-patch`: fails on stable with E0554 in `wasm_thread` (`#![feature]` on the stable channel). The same failure happens with the feature off, so it is not caused by this change. `web/build.sh` builds with `RUSTC_BOOTSTRAP=1` (see development.md, Web build). With `RUSTC_BOOTSTRAP=1` the command passes. `cargo tree --target wasm32-unknown-unknown --features hot-patch -e normal` lists no `subsecond`, `dioxus*` or `async-tungstenite`, and `-i subsecond` prints nothing. The only warning is the existing `tip_with_key` dead-code warning (`src/ui/kit.rs:498`), also present with the feature off.
    - `Cargo.lock`: 112 added lines, no removals. New packages (9): `async-tungstenite 0.33.0`, `dioxus-cli-config 0.7.10`, `dioxus-core 0.7.10`, `dioxus-core-types 0.7.10`, `dioxus-devtools-types 0.7.10`, `generational-box 0.7.10`, `longest-increasing-subsequence 0.1.0`, `subsecond 0.7.10`, `subsecond-types 0.7.10`. The other added lines are dependency edges in the existing `gpui-pre` entry (`async-tungstenite`, `dioxus-cli-config`, `dioxus-devtools-types`, `smol`, `subsecond`) and the `querymt-gpui` entry (`gpui-pre`).
- [x] 2.3 Add `crates/querymt-gpui/scripts/hot-patch.sh` (design D2):
  - resolves `$QMT_DX`, else `$CARGO_HOME/bin/dx`;
  - refuses any version other than 0.7.10 with the install hint;
  - runs `dx serve` with the flags recorded in 1.2, `-p querymt-gpui` and `--features hot-patch`, passing the script's arguments to the app;
  - `set -euo pipefail`, executable bit set.
  - Verify:
    - `bash -n` passes, and `shellcheck` passes if installed;
    - with `QMT_DX=/opt/homebrew/bin/dx` (Deno's) the script exits 1 with the install hint;
    - with the real CLI it starts the app, and the log shows `GPUI hot reload connected` (not the "no dx development server" warning).
  - Record:
    - `bash -n` and `shellcheck` (installed) pass. Mode is `-rwxr-xr-x`.
    - `QMT_DX=/opt/homebrew/bin/dx scripts/hot-patch.sh`: exit 1, `is not the Dioxus CLI 0.7.10 (it reports: deno 2.9.8 ...)` plus the install hint. A missing `QMT_DX` path: exit 1 with the same hint. An app argument containing whitespace: exit 1.
    - Real CLI: `scripts/hot-patch.sh --page sessions` runs `dx serve --hot-patch --platform macos --open false --always-on-top false -p querymt-gpui --bin querymt-gpui --features hot-patch --args=--page sessions`. The app starts as `target/dx/querymt-gpui/debug/macos/QuerymtGpui.app/...` with `--page sessions`.
    - **Deviation, the log line cannot be observed:** the app installs a `tracing` subscriber with no `log` bridge (`src/telemetry.rs:143-172`, no `LogTracer`) and no `log` logger exists in the app, gpui-pre or gpui-kit, while gpui-pre logs through `log::info!` (`gpui-pre-0.3.14/src/hot_reload.rs:74`). So neither `GPUI hot reload connected` nor the "no dx development server" warning can appear in any log, whatever `RUST_LOG` is. Design D5 forbids app changes, so connection was verified another way: the app process has `DIOXUS_CLI_ENABLED=true` and `DIOXUS_DEVSERVER_IP/PORT=127.0.0.1:8080` in its environment, and `lsof` shows an ESTABLISHED connection `127.0.0.1:<port> -> 127.0.0.1:8080` (dx's listener) from the app, on a cold run, a touched run and the final warm run. That socket is the websocket opened by `hot_reload::init`. No patch was applied in this group; that is task 3.2.
    - **Fix found during verification:** a warm start (nothing to recompile) fails with `Build failed: Missing linker args for fat link of 'querymt_gpui'` in about 2.4 s, with sccache on and with `RUSTC_WRAPPER=` unset, so it is not sccache. After `touch src/main.rs` the build completes (7.3 s). The script now touches `src/main.rs` before `dx serve`. Final run of the script with no manual touch, sccache on: `Build completed successfully in 5.38s`, app connected.
    - Observation for 3.1 (not the measurement): the first run on a target dir without a dx build took `Build completed successfully in 61.82s`.
    - Observation: `dx serve` leaves the app process running after dx itself is killed; stop `QuerymtGpui.app` too.

## 3. Measure and decide

- [x] 3.1 Record the time from `scripts/hot-patch.sh` to the main window appearing, cold (after `cargo clean -p querymt-gpui`) and warm.
  - If the build fails, retry once with `RUSTC_WRAPPER=` (design D3); if that fixes it, the script unsets it for `dx`.
  - Verify: both times and the sccache outcome are recorded below this task.
  - Record (2026-11-26, arm64 macOS, rustc 1.100.0, `dx 0.7.10`, `RUSTC_WRAPPER=sccache`, `scripts/hot-patch.sh --page sessions`, measured by a script, not by eye):
    - **Where dx builds:** `target/desktop-dev` and `target/aarch64-apple-darwin/desktop-dev`, with captured linker args in `target/dx/.captured-args`. This is not the `target/debug` that `cargo clean -p querymt-gpui` cleans (`cargo clean --profile desktop-dev` fails: `profile desktop-dev is not defined`). The first attempt, after `cargo clean -p querymt-gpui`, was therefore warm (8.6 s). The cold run below removed the package's artifacts from dx's dirs by hand: `incremental/querymt_gpui-*`, the binary and `.d`, `build/querymt-gpui`, `deps/querymt_gpui-*`, `.fingerprint/querymt-gpui-*`, `target/dx/.captured-args/querymt_gpui-*` and `target/dx/querymt-gpui`. Dependencies stayed built.
    - **Cold (tip crate rebuilt):** `Build completed successfully in 12.89s`, 15 s wall from script start; telemetry init (app running) 17.6 s on dx's clock; first rendered-frame log line (`slow app render`) at 22.5 s.
    - **Warm (touch + incremental):** `Build completed successfully in 5.23s`, 9 s wall; telemetry init at 8.9 s; first rendered-frame log line at 13.9 s.
    - **Not measured:** the window itself appearing. The times above are the app's own first log lines, which are 5 s apart from launch to first frame in both runs. Read them as an upper bound.
    - **sccache:** no effect. After `sccache --zero-stats`, a full cold run shows `Compile requests executed 0`, `Cache hits 0`, `Non-cacheable calls 11`. dx's builds are incremental, which sccache does not cache. No failure with it on, so the script does not unset `RUSTC_WRAPPER`.
    - **Earlier first build on a dx target dir with no dependencies built:** `61.82s` (for reference only).
    - The default `[profile.dev]` is used as is.
- [x] 3.2 With the app open on a session (`--page sessions`, open one session), make the three edits below and record, for each, the time from save to the window updating and whether the session, scroll position and connection survived:
  - (a) change a padding or colour in a `src/ui/session_view.rs` render function;
  - (b) add a button with an `on_click` that logs, in the same render;
  - (c) add a field to a long-lived struct (for example `SessionPane`), expecting a crash or restart.
  - Verify: three result lines are recorded. (a) and (b) update without a restart, or the failure is recorded with the dx output.
  - Record (2026-11-26, interactive run: the user watched the window and answered questions, patch times come from dx's log; every edit was reverted with `git checkout` afterwards). The scripted first run, with no session open, gave the same patch times.
    - (a) `session_view.rs`, user-message bubble `py` 8 -> 20: `Hot-patching: crates/querymt-gpui/src/ui/session_view.rs took 2066ms`, logged 2.3 s after the save (scripted run: 2043 ms). The user saw taller bubbles; the same session stayed open, and scroll position and connection were intact. No restart.
    - (b) same render, a `div().id("hot-patch-test")` child with `.on_click(|_, _, _| tracing::error!("hot-patch test click"))`: `took 3200ms`, 3.6 s after the save (scripted run: 3193 ms). The user saw the label and clicked it; the log shows `hot-patch test click` from the patched handler (twice). Session and connection survived, no restart.
    - (c) `SessionPane` gained `hp_probe: u64` (struct and constructor), with a session open: `Hot-patching: crates/querymt-gpui/src/ui/session_pane.rs took 4157ms`, then within 0.3 s `thread 'main' panicked at gpui-pre-0.3.14/src/app/entity_map.rs:184:32: cannot read querymt_gpui::ui::session_pane::SessionPane while it is already being updated` and `Application [macos] exited with error: exit status: 101`. The user confirmed the app was gone, so the session was lost. This is the expected crash, so D4 keeps "restart after changing a struct". The scripted run with no session open did not crash (5037 ms, process alive 20 s later): the crash needs a live `SessionPane`.
    - The app only logs at `error` by default and has no `log` bridge, so `GPUI hot reload applied` never shows (see 2.3). dx's `Hot-patching: ... took Nms` line is the signal.
- [x] 3.3 Only if 3.2 (a) took more than about 5 s, or the debug app is unusably slow, try the `dev-hot` profile from design D3, if dx accepts a profile (per 1.2). Rerun 3.2 (a).
  - Verify: the numbers are recorded. The profile is committed to the root `Cargo.toml` only if it helps; otherwise it is removed and the outcome is recorded.
  - Record: not needed, so the profile was not tried and the root `Cargo.toml` is unchanged. 3.2 (a) took about 2 s (under the 5 s threshold), and the user, asked directly, said the debug app was not unusably slow (the app does log `slow app render` warnings of 16 to 27 ms in debug).
- [x] 3.4 Check the remaining branches of design D4 with one edit each and record the result:
  - a key binding change in `src/ui/keys.rs`;
  - a change in `crates/agent-session-core` (expected: not patched);
  - opening the quick panel after a patch;
  - a Mermaid or code block re-rendering after a patch (thread-local caches).
  - Verify: four result lines are recorded below this task.
  - Record (interactive runs on 2026-11-26, the user watching the window; each edit reverted afterwards):
    - Key binding: `keys.rs` `ToggleSidebar` Cmd+B -> Cmd+J. `Hot-patching: .../keys.rs took 1939ms`. The user pressed both: only Cmd+B (the old binding) toggled the sidebar and Cmd+J did nothing. Bindings are registered once at startup (`bind_keys`), so the old one stays until restart, as D4 says.
    - `crates/agent-session-core`: `core.rs` `format!("{} frames", ...)` changed. dx: `Build failed: Missing rustc args for replay: 'querymt_agent_session_core'` 0.3 s after the save. Not patched; the app kept running unchanged (same PID). **Side effect:** dx then failed the same way for every later edit, including the revert and a plain `session_view.rs` edit, and a further touch of `session_view.rs` logged nothing. dx did not recover in the same session, so a change outside the app crate needs a restart of `hot-patch.sh` (the `r` rebuild key was not tried).
    - Quick panel: **works after patches.** The user's other copy of the app (a release build, PID 18439, the app they were talking to) held the global hotkey and the `--quick` socket and could not be quit. The dev copy was therefore started as `USER=hpqp crates/querymt-gpui/scripts/hot-patch.sh --page sessions`: the socket name is built from `$USER` (`src/quick/instance.rs`), so it listened on its own `/tmp/querymt-gpui-hpqp`, and `USER=hpqp target/dx/querymt-gpui/debug/macos/QuerymtGpui.app/Contents/MacOS/querymt-gpui --quick` forwarded to it and exited 0 (the dev copy's PID unchanged). The hotkey itself was not registered by the dev copy (the other copy holds it), so the hotkey path is untested.
      - After a patch to `src/ui/session_view.rs` the user opened the panel: it opened and looked right (material, rim light).
      - Patching the panel's own code: `src/quick/panel.rs` `render_picker` list gap `gap_1()` -> `gap(px(20.))` (`took 2009ms`), then reopening the panel: the user saw the session rows spaced wider and the chrome fine. Two earlier panel edits were not observable, because the panel has no header row and no title in its default state (the header fallback title `"Quick chat"` -> `"Quick chat HP"`, `took 1976ms`; the header `px_3` -> `px(40.)`, `took 1975ms`), so they say nothing either way.
      - Not tried: patching while the panel is open, and a patch followed by the hotkey.
    - Mermaid: after a render patch the diagram was still drawn correctly, before and after scrolling it off-screen and back.
    - Code block: after a render patch (`session_view.rs`, `took 2069ms`) the user's code block was still drawn with its text and syntax colours, before and after scrolling off-screen and back. That edit did not touch the code block, so it only showed that it survives. **Retested with a change to the code block itself:** in `src/ui/md.rs` `styled`, the markdown `code_block` style went from `.rounded(px(8.)).pt(px(42.)).pb_3().text_size(px(13.))` to `.rounded(px(40.)).border_4().border_color(rgb(0xff0000)).pt(px(42.)).pb_3().text_size(px(26.))`. `Hot-patching: crates/querymt-gpui/src/ui/md.rs took 2976ms`. The user saw the thick red border, round corners and double-size text immediately, without scrolling. Reverting it (`took 2938ms`) put the code block back to normal. Same PID throughout, no restart.
- [x] 3.5 Record the go or no-go:
  - go: keep the feature and docs;
  - no-go: report to the user with the numbers, and remove or keep the feature as they decide.
  - Verify: the decision and its reason are recorded below this task.
  - Record: **go**, decided by the user after the 3.2 and 3.4 results. Reason: render edits patch in 2 to 3 s and keep the session, scroll position and connection; the known limits (struct changes crash, startup code and key bindings need a restart, edits outside the app crate need a restart) are documented. Kept as opt-in tooling. (The quick panel and code-block checks of 3.4 were completed afterwards and also passed.)

## 4. Docs

- [x] 4.1 Add a "Hot-patching the UI" section to `crates/querymt-gpui/knowledge/development.md`:
  - installing dx 0.7.10 and the Deno `dx` clash;
  - `scripts/hot-patch.sh [app args]`;
  - the measured latency;
  - the D4 table of what patches and what restarts, updated with the 3.2 and 3.4 results;
  - the sccache note, if any;
  - keeping `gpui-pre` and the dx version in step with gpui-kit bumps (`cargo check -p querymt-gpui --features hot-patch`).
  - Add `crates/querymt-gpui/scripts/hot-patch.sh` to the note's `knowledge_paths`.
  - Verify:
    - the documented command runs as written;
    - `cargo run -p querymt-agent --example repository_knowledge -- --root . check` passes.
  - Record (2026-11-26):
    - Added the section "Hot-patching the UI" after "Web build" in `crates/querymt-gpui/knowledge/development.md`, with install, the script, the script's pitfalls (one `--args` string, the touch of `src/main.rs`, `target/desktop-dev`, sccache, killing dx leaves the app running), the measured timings, the D4 table filled from 3.2 and 3.4, and the pin and CLI-version upkeep. It marks code blocks and the quick panel as not tried. It also added `hot-patch` tags and `crates/querymt-gpui/scripts/hot-patch.sh` to `knowledge_paths`.
    - The check first warned `knowledge_path_unmatched: crates/querymt-gpui/scripts/hot-patch.sh matches no indexed file`, so the script was added to `[code] include` in `.qmt/knowledge.toml` (next to `web/build.sh`). That warning is gone.
    - **The check still exits 1**, only because of 6 warnings (`evidence_changed`, `evidence_entry_orphaned`, `evidence_unverified`) in `crates/agent-runtime/knowledge/execution.md`, a file this change does not touch (it was already out of date; not compared on a clean `HEAD`). Nothing in the output mentions `querymt-gpui` or `hot-patch`.
    - Both documented commands run as written: `crates/querymt-gpui/scripts/hot-patch.sh` (build 8.8 s, app started, connected to dx) and `crates/querymt-gpui/scripts/hot-patch.sh --agent 127.0.0.1:42069 --page sessions` (build 4.5 s; dx got `--args=--agent 127.0.0.1:42069 --page sessions` and the app started with those arguments, connected to dx).

## 5. Package checks

Run on 2026-11-26 (arm64 macOS, rustc 1.100.0, `RUSTC_WRAPPER=sccache`, cargo-nextest 0.9.136) after Groups 1 and 2. Re-run 5.1 to 5.4 if Group 3 or 4 changes `Cargo.toml` or Rust code (for example the `dev-hot` profile in 3.3).

- [x] 5.1 `cargo check -p querymt-gpui` passes, and so does `cargo check -p querymt-gpui --features hot-patch`.
  - Both finished with exit 0.
- [x] 5.2 `cargo t-gpui` passes. Record the pass/skip counts. The feature is off, so the result should match `main`.
  - `cargo t-gpui` (`nextest run -p querymt-gpui --bins`): `729 tests run: 729 passed, 7 skipped`. Not compared against a `main` run: the feature is off and this change touches no Rust code, only `Cargo.toml`, `Cargo.lock` and a script.
- [x] 5.3 `cargo fmt -p querymt-gpui -- --check` passes.
  - Exit 0, no output.
- [x] 5.4 `cargo cl-gpui` passes, and so does `cargo clippy -p querymt-gpui --all-targets --features hot-patch -- -D warnings`.
  - Both finished with exit 0.
- [x] 5.5 `openspec validate add-gpui-hot-patch --strict` passes.
  - `Change 'add-gpui-hot-patch' is valid` (`skip_specs` info only).

## Workflow follow-up

- If upstream gpui-kit gains a `hot-reload` feature, replace the direct `gpui-pre` dependency with it (design D1).
- Consider adding dioxus-cli 0.7.10 to the `flake.nix` devShell if the feature is kept.
- Archive this change after review.
