# AGENTS.md

## Preferences
- Developer preferences are high priority; follow them strictly.
- If you are being corrected by developer while doing something or asked to redo, extract the preference and add it here.
- Keep changes minimal and focused; avoid drive-by refactors.
- Use `log`/`tracing` macros (match the surrounding crate) instead of `println!/eprintln!`.
- After any Rust code changes, run `cargo check` (target the smallest package possible, e.g. `-p querymt-cli`).
- Run `cargo fmt` only after `cargo check` passes.
- Add tests when introducing behavior changes if the crate has an existing test pattern.

## Repository Overview
- Language: Rust workspace (mixed editions; workspace defaults to 2024 but some crates may pin 2021).
- Workspace root: `Cargo.toml` (members under `crates/`).
- Key packages:
  - `querymt` (core library)
  - `querymt-cli`
  - `qmt-agent` (agent + optional UI under `crates/agent/ui`)
  - `querymt-service` (Axum service; bin `qmt-service`)
  - Provider crates under `crates/providers/*`
- Toolchain: `rust-toolchain.toml` (stable + clippy + rustfmt; includes `wasm32-wasip1`).

## Build, Lint, and Test Commands

### Workspace
- Check: `cargo check`
- Format: `cargo fmt --all`
- Format (CI-style): `cargo fmt --all -- --check`
- Lint: `cargo clippy --all-targets -- -D warnings`
- Tests: `cargo test`

### Package-scoped (preferred for iteration)
- `cargo check -p querymt`
- `cargo check -p querymt-cli`
- `cargo check -p qmt-agent`
- `cargo check -p querymt-service`

### Agent crate (matches CI)
- `cargo clippy -p qmt-agent --all-targets --features dashboard,oauth -- -D warnings`

### UI (agent dashboard)
- `cd crates/agent/ui && bun install`
- `cd crates/agent/ui && bun test`

## Code Style Guidelines

### Formatting
- Use `rustfmt` defaults.
- Prefer readable code over manual alignment.

### Imports
- Group imports: `std` first, then external crates, then `crate`/`super`.
- Avoid glob imports in library code.

### Error Handling
- Prefer propagating errors with `?`.
- Avoid `unwrap/expect` in non-test code.
- `anyhow::Result` is fine for binaries/examples; libraries should use their existing error conventions.

### Logging and Tracing
- Prefer structured logging via `log::*` or `tracing::*` depending on the crate.
- Include enough context to debug provider/service failures (model, provider, request id) but avoid logging secrets.

### Async
- Prefer Tokio for async entry points.
- Avoid blocking in async contexts; use `spawn_blocking` when needed.

### API Surface
- Keep public APIs small; use `pub(crate)` for internals.
- Avoid changing wire formats/config fields unless explicitly requested.
