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
- `querymt-agent`: agent runtime and optional embedded dashboard.
- `querymt-remote`: shared mesh and remote-provider primitives.
- `querymt-service-client`: service OAuth and API client primitives, not a server.
- Provider crates live under `crates/providers/`.

## Development Environment

- Detect Nix first. When available, use the project devShell (`nix develop` or
  `nix develop -c <command>`); an active project direnv shell is sufficient.
- Without Nix, use the supported native toolchain and dependencies. Check `flake.nix`
  for supported Nix platforms; diagnose setup failures rather than silently
  switching toolchains or environments.
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
  cargo test -p <package> <test_filter>
  cargo fmt -p <package> -- --check
  cargo clippy -p <package> --all-targets -- -D warnings
  ```
- Expand validation to affected consumers and feature combinations. Consult the
  relevant `.github/workflows/` file for CI commands/features and `.config/nextest.toml`
  for test timeouts.
- Whole-workspace checks are optional broader validation, not the default iteration
  loop. Native providers can require expensive builds or model downloads; inspect
  their workflows before selecting tests. Do not apply `--all-features` indiscriminately.
- Report checks actually run, failures and remaining gaps. Do not describe partial
  validation as complete. Documentation-only edits do not require a Rust build.

## Dashboard Boundary

- UI implementation and testing belong to the separate `querymt-desktop` repository.
  Report required UI follow-up rather than expanding backend work into that repo.
- For backend-only checks with `dashboard` enabled, use the `QMT_UI_DIST` fixture
  in `.github/workflows/test-agent.yml` instead of rebuilding the frontend.

## Code Conventions

- Follow surrounding code, rustfmt and existing error conventions.
- Use the surrounding crate's `log`/`tracing` conventions for diagnostics; preserve
  intentional CLI/protocol output. Include useful failure context without secrets.
- Implement backward compatibility only when explicitly requested by the user.
  Do not add compatibility layers or preserve legacy behavior by default. If a change
  appears to require backward compatibility, flag it for user review before implementing it.
