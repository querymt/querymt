# Proposal

## Why

`querymt-agent` is one crate of about 314k lines: about 162k production and 152k test, with roughly 2,900 tests. Merged unsplit, `feat/kb` would take it to about 345k lines; this change instead lands the knowledge engine as its own crate, so `querymt-agent` never absorbs those 26k lines. rustc builds a crate on a single front-end thread, so every test run recompiles everything. A test-mode build of the library alone took 121 s on a cached dependency graph. `cargo clippy --all-targets` took 81 s, and nothing can be tested in isolation. 36 of the 46 top-level modules form one dependency cycle, so the crate cannot be split along its current module lines. This change untangles the cycle and slices the crate into layered crates. Then a change to one layer compiles and tests only that layer and the layers above it.

## What Changes

- Split `crates/agent` into seven layered packages. The new ones live in `crates/agent-<layer>`. `querymt-agent` stays in `crates/agent` as the top package and keeps its features and public module paths:
  - `querymt-agent-core`: the bottom crate. Domain and event types, `hash`, configuration leaf types, and every storage contract, including the knowledge store contract
  - `querymt-agent-index`: the code index (tree-sitter, `syn`)
  - `querymt-agent-sqlite`: all session-database SQLite code (schema, migrations, stores, repositories)
  - `querymt-agent-knowledge`: the repository-knowledge engine from `feat/kb` (`knowledge/repository/`) and its settings types (`RepositoryKnowledgeSettings`, `RepositoryKnowledgeDeliverySettings`, the `REPOSITORY_KNOWLEDGE_*` constants). It depends only on core and index, and keeps its own `index.sqlite` file
  - `querymt-agent-services`: accounting, worktree, snapshot, export, tools, event fan-out, session services
  - `querymt-agent-runtime`: agent, ACP core, control, delegation, middleware, hooks, config, dotagents, scheduler, skills, session runtime
  - `querymt-agent`: API, ACP WebSocket server, dashboard, quorum, runner, profile runtime manager and embedded profile catalog, and re-exports of the whole tree
- Land the parts that do not depend on the storage rework first. The code index, and the few leaf types that `knowledge/repository` needs, move before `feat/kb` merges. Then `feat/kb` rebases and merges already split into `crates/agent-knowledge`.
- Remove every upward module dependency before each extraction. This moves plain types and helpers down and replaces five behaviour back-edges with a trait or a removal. A temporary layering check in CI blocks new upward dependencies while the migration runs.
- Storage follows a contract/implementation split. Only `querymt-agent-sqlite` depends on `rusqlite` for the session database. Production code below the top package uses only the storage traits. Tests in lower crates use the SQLite implementation as a dev-dependency.
- Test helpers move out of the single `#[cfg(test)]` module into per-crate `test-support` features, so upper crates' tests can reuse lower crates' fixtures.
- CI tests and lints each package separately. Path filters cover the new directories. The typeshare script reads the new crates.
- Old paths keep working (user-approved):
  - **External paths:** `querymt-agent` re-exports the same module tree (`querymt_agent::session::…`, `querymt_agent::acp::protocol`, `querymt_agent::api::Agent`, …).
  - **Internal paths:** temporary `crate::x` aliases inside moved code are removed in the last phase.
- **BREAKING (Rust API):** `ViewStore` loses `get_atif`. ATIF export becomes a function over the existing audit-view query. No workspace package calls `get_atif`.
- **BREAKING (Rust API):** `SessionError::Database` no longer wraps `rusqlite::Error`. It holds a database-neutral `DbError` (boxed source plus a `NoRows` / `Busy` / `Other` kind), so core stays free of `rusqlite`. `is_no_rows()`, `is_transient()` and the `source()` chain behave as in `harden-agent-types-and-errors`. SQLite code classifies its errors with an extension trait. `KnowledgeError` no longer implements `From<rusqlite::Error>` and keeps its string variant.
- **BREAKING (log filtering):** `log` and `tracing` targets come from module paths, so they gain the new crate prefix. For example, `querymt_agent::agent::remote` becomes `querymt_agent_runtime::agent::remote`. A filter on the bare crate name (`querymt_agent=debug`) is a string-prefix match and still covers every crate; this must be verified. Filters on deeper paths, like the one in `docs/docs/agent/mesh.md`, need the new path. Docs and CHANGELOG are updated.
- No other behaviour change. Config, hooks, middleware, ACP and provider wire shapes, kameo remote actor IDs, the Extism contract, the session database schema and migrations, Python bindings and typeshare output all stay identical.

## Non-goals

- Splitting the remaining runtime core (agent, session actor, ACP core, control) further, including extracting the `remote` mesh code. This waits until the measurements after this change.
- Behaviour changes, performance work inside the code, or new features.
- Changes to the session database schema, or new migrations.
- Toolchain or profile changes (nightly `-Zthreads`, Cranelift, linker or debuginfo settings).
- A pluggable non-SQLite storage backend. The trait split makes one possible, but none is added.

## Capabilities

### New Capabilities

None. This is a structural refactor with no spec-level behaviour change, so the change sets `skip_specs: true`.

### Modified Capabilities

None.

## Impact

**Packages and features:**

| Package | Status | Features whose code moves |
|---|---|---|
| `querymt-agent` | Shrinks to the top layer and re-export facade; keeps every feature | `api`, `dashboard`, `plugin-loaders` |
| `querymt-agent-index` | New | none |
| `querymt-agent-core` | New | none |
| `querymt-agent-sqlite` | New | none |
| `querymt-agent-knowledge` | New. Depends on core and index only, with no storage-contract or sqlite dev-dependency | none |
| `querymt-agent-services` | New | none |
| `querymt-agent-runtime` | New | `remote`, `oauth`, `plugin-loaders`, `api`, each forwarded from `querymt-agent`. `zbus-secret-service`, `systemd-credentials`, `agent-mcp` and `agent-tracing` only forward to `querymt-utils` or `querymt` and stay on `querymt-agent` |
| `querymt-agent-py`, `querymt-mobile-ffi` | No source changes expected; must still build | none |

**Public Rust API:**
- Module paths under `querymt_agent::` are unchanged.
- **BREAKING:** log and tracing targets get the new crate prefixes, as described in What Changes.
- **BREAKING:** `ViewStore::get_atif` is removed.
- **BREAKING:** `SessionError::Database` carries `DbError` instead of `rusqlite::Error`, and `From<rusqlite::Error>` for `SessionError` and `KnowledgeError` is removed.
- Items that are `pub(crate)` today and cross a new crate boundary become `pub` with `#[doc(hidden)]`.

**Unchanged contracts:** config, hooks, middleware, ACP, provider wire shapes, the Extism contract, the session DB schema, Python bindings and typeshare types. typeshare output is diffed to prove it is identical.

**Tooling and docs to update:**
- `Cargo.toml` workspace members
- `.github/workflows/test-agent.yml`: per-package clippy and coverage, the layering check, and path filters
- `docs/docs/agent/mesh.md`: log filter example
- `.github/workflows/test-python-packages.yml`: path filters
- `scripts/generate-types.sh`
- `AGENTS.md` and `openspec/config.yaml`: package list and test-scope guidance
- Doc-sync and fixture tests that read files by relative path

**Coordination:**
- **Before `feat/kb` merges:** Phases 0 and 1 (baseline, layering check, core seed, index) and the leaf-type seed for knowledge (tasks 1.6 to 1.8). `feat/kb` then rebases and moves `knowledge/repository/` into `crates/agent-knowledge` (task 1.9). Of its 33k added lines, 26k sit in that directory; the rest is in `agent`, `config`, `tools` and `api` and stays in `querymt-agent` for now.
- **After the storage changes:** Phase 2 onward (core contracts, sqlite, services, runtime) stays sequenced after `harden-agent-types-and-errors` (changes `SessionError`, the sqlite storage layer, `event_sink.rs`, `session_event_stream.rs` and typeshare output) and after the in-flight session-storage changes `speed-up-session-loading`, `ordered-session-events` and `add-git-worktree-isolation`.
- `fix/snapshot` is already merged (`9acecd3b`) and is no longer a gate.
- `harden-agent-types-and-errors` also edits 8 files in `index/` and `hooks/engine.rs`. The index move commit contains renames only, so that branch rebases over it.
- Every extraction phase rebases or moves code that other open branches touch.

**UI:** no querymt-desktop follow-up is needed, because ACP payloads and typeshare types are unchanged.
