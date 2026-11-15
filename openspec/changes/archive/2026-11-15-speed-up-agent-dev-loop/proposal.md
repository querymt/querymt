# Proposal

## Why

After the agent crate split, the edit–build–test loop feels slow and memory-hungry, and the numbers recorded in `split-agent-crate` 9.3 made it look twice as slow as before. Re-measuring on the same compiler showed the 75 s builds came from `CARGO_INCREMENTAL=0`, not from the split. Real costs remain, though:

- A services edit that shifts line numbers costs about 55 s in the facade test build.
- Switching between package, feature or profile sets recompiles 130–980 third-party crates.
- The facade test build peaks at about 22 compiler/linker processes and 17–20 GB.
- The Homebrew `rust` formula shadows rustup, so `rust-toolchain.toml` is ignored and every `brew upgrade` forces a cold rebuild.

This change fixes the environment and conventions first, measures the two open questions, and then applies only the structural fixes the measurements support.

## What Changes

- **Toolchain:** use rustup (already installed at `/opt/homebrew/opt/rustup/bin`) instead of the Homebrew `rust` formula, so `rust-toolchain.toml` is honoured and `cargo +nightly` works. Documented in `AGENTS.md`.
- **Incremental compilation:** document that `CARGO_INCREMENTAL` stays unset for local work. `=0` costs about 60 s per edit, and `=1` makes sccache refuse to run. A `scripts/` cleanup helper keeps `target/` size in check, since disk growth was the reason incremental had been turned off.
- **Standard commands:** a new `.cargo/config.toml` with aliases that use the CI feature set for each agent package, so local runs share build artifacts with each other and with CI's flags.
- **Parallelism:** measure the facade test build at `-j 14`, `-j 8` and `-j 6`, and document a local default if it lowers peak memory at little wall-time cost.
- **Measurement script:** a script that reproduces the edit-loop measurements (probe edit, wall time, peak memory), so later changes can compare against the same method.
- **Diagnostics:** find out why line shifts in services cost 55 s, measure a core edit, and measure `cargo hakari` in a scratch worktree.
- **Conditional structure:**
  - merge the seven `crates/agent/tests/*.rs` integration tests into one binary;
  - adopt a workspace-hack crate only if the measurement shows a clear saving;
  - drop the facade's dev-dependency on runtime `test-support` if few facade tests need it.
- **Correction:** the `split-agent-crate` 9.3 note already states that its timings were taken with incremental off.

No public Rust API, config, hook, middleware, ACP, provider wire shape, Extism contract, session DB schema, Python binding or typeshare type changes. The test count stays the same; only integration-test binary names may change (see Impact).

## Non-goals

- Splitting the runtime crate into parallel sibling crates (`remote`, `acp`, `dotagents`). This overlaps `unify-agent-runtime` and gets its own change.
- Code-level compile-time work (boxing large futures, reducing exported generics), which needs `-Zself-profile` data first.
- Dynamic linking of dependencies, Cranelift, `-Zthreads`, `debug = false` for dependencies and `build-override` opt-level. Measured or analysed as not helping: linking is under 0.2 s for the runtime rlib, proc-macro expansion is 1.1 s of 57 s, and `debug = false` changed only disk size.
- Pinning `rust-toolchain.toml` to an exact version, and changes to CI toolchains or CI caching.
- Editor or rust-analyzer configuration (no editor is in use).

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None. This is a tooling, documentation and test-layout change with no behaviour change, so the change sets `skip_specs: true`.

## Impact

- **Packages:** `querymt-agent` (tests layout, possibly `[dev-dependencies]`), possibly `querymt-agent-runtime` (receiving moved tests). A workspace-hack package only if 3.2 decides for it. No feature flag changes code.
- **Repository files:** `AGENTS.md`, `openspec/config.yaml`, new `.cargo/config.toml`, new scripts under `scripts/`, `crates/agent/tests/`, possibly `Cargo.toml` (workspace member) and `.github/workflows/test-agent.yml` (only if the hakari check is adopted).
- **Test names:** merging the integration tests changes nextest binary IDs from `querymt-agent::<file>` to `querymt-agent::it`. Test function names and `.config/nextest.toml` filters (which match on test names) are unaffected. `openspec/changes/add-portable-repository-knowledge/tasks.md` references `--test repository_knowledge_matrix` and is updated.
- **Special builds:** none. wasm32-wasip1 provider plugins, `querymt-agent-py` and `querymt-mobile-ffi` build the same code.
- **Developer environment:** removing or unlinking the Homebrew `rust` formula is a manual step on the developer's machine.
- **querymt-desktop:** no UI follow-up.
