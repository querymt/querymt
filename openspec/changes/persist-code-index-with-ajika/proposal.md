# Proposal

## Why

The agent's code index throws its work away. When a workspace index starts (session materialization) or rebuilds, `FunctionIndex::build` re-reads and re-parses every source file with tree-sitter/syn, and nothing survives a process restart. `FunctionIndexConfig::cache_path` exists for this but nothing reads it. The symbol tools (`index`, `get_symbol`, `get_function`, `replace_symbol`, `find_symbol_references`) also re-parse on every call. `find_symbol_references` on a directory re-parses every matching file each time, even when nothing changed.

[Ajika](https://github.com/rust-glancer/ajika) (0.2.1, MIT/Apache-2.0) is a persistent incremental query library. It remembers per-file derived results and their dependencies, revalidates them against fresh inputs after a restart, and keeps results on disk instead of in RAM. The code index fits it well: the expensive work is deterministic, keyed by file, and recomputable.

## What Changes

- Each workspace index owns one Ajika database. It holds two per-file queries:
  - **symbols**: the shared symbol extraction behind the symbol tools and outlines.
  - **functions**: the function entries behind duplicate detection.

  The input to both is the file text that the caller supplies.
- The function index is built from the `functions` query. After a restart, unchanged files are validated instead of re-parsed. Created and modified files from the file watcher are installed as one input batch per change set.
- The five symbol tools read symbols through the workspace index when it is ready. Each tool still reads the file itself and supplies the text it read, so results always match the file content seen by that call. A file read by one tool call is parsed at most once for that call. A file whose text did not change since an earlier call is not parsed again.
- The tools fall back to direct parsing (today's path) when:
  - the index is not ready,
  - the index does not answer within a short deadline, or
  - the cache reports an error.

  Tool output is identical either way.
- The persistent cache lives under the platform cache directory at `querymt/code-index/<workspace hash>`. It is bounded in size and idle age, and it is discarded when the index schema version changes.
- If the directory is locked by another process, cannot be created or cannot be opened, the index uses an in-memory Ajika database for that workspace and logs a warning.
- Workspace index statistics report how many file parses ran and how many cached file results were reused since the index started, counting both indexing and tool lookups.
- The cache runs on one dedicated owner thread per workspace, which serializes indexing and lookups. Unparseable files are recorded as per-file results, so they never fail a whole indexing pass.
- **BREAKING** (Rust API, `querymt-agent-index`):
  - `WorkspaceIndexManagerConfig` gains a `storage` field.
  - `WorkspaceIndexStats` gains `parsed_files` and `reused_files`.
  - The unused `FunctionIndexConfig::cache_path` and `FunctionIndexConfig::with_cache_path` are removed.
- New public Rust API (non-breaking, `querymt-agent-services`): the `ToolContext` method `workspace_index()` returns `None` by default.

No tool output, tool schema, config key, hook, middleware, ACP payload, session DB schema, typeshare type or plugin contract changes.

## Non-goals

- **Repository knowledge** (`querymt-agent-knowledge`): its reconcile pipeline, its SQLite store and its per-pass `DependencyCache` stay as they are. Sharing the symbols query with `extract_code` is a possible follow-up.
- **Lower memory for duplicate search**: `find_similar` still scans every function held in memory. Bucketing by structural fingerprint, so entries can stay on disk, is a follow-up.
- **Snapshot and diff scanning** (`StatCache`, `MerkleTree::scan_with_previous`): their cost is change detection, which Ajika leaves to the host.
- **Configuration**: no TOML key to disable or relocate the cache. Deleting the directory is safe, and tests use the Rust-level `storage` setting.
- **Shared cache across processes**: one Ajika owner per directory. Other processes use memory.
- **Other derived data**: LLM-dependent or time-dependent results (compaction, summaries, model inventory) are out of scope.

## Capabilities

### New Capabilities

- `code-index-cache`: persistence, freshness, fallback and storage behaviour of the agent's per-workspace code index and the symbol tools that read through it.

### Modified Capabilities

None. The `index-tool` outline contract is unchanged; `code-index-cache` requires cached results to equal uncached ones.

## Impact

- **Packages:**
  - `querymt-agent-index`: Ajika database, queries, persisted value types, storage selection, stats. No feature flags.
  - `querymt-agent-services`: `ToolContext::workspace_index` and the five symbol tools. No feature flags.
  - `querymt-agent-runtime`: passes the session's workspace handle into the tool context. Builds with CI features `remote,oauth,plugin-loaders,api`; no feature-gated code changes.
  - Downstream builds only: `querymt-agent`, `querymt-agent-knowledge`, `querymt-agent-py`, `querymt-mobile-ffi`.
- **Dependencies:**
  - New crate-local dependency `ajika = "0.2"` in `querymt-agent-index`. It brings redb 4, zstd 0.14, lz4_flex 0.14, wincode, inventory and twox-hash.
  - redb, zstd and lz4_flex will exist in two versions alongside the current redb 3 / zstd 0.13 / lz4_flex 0.13.
  - `dirs` (workspace) is added to `querymt-agent-index`.
  - Requires `cargo hakari generate` and `cargo hakari manage-deps`.
  - Provider crates and `querymt` do not depend on `querymt-agent-index`, so wasm32-wasip1 builds are unaffected.
- **Persisted state:** a new disposable cache directory per workspace root. It is not part of the session database and has no migration.
- **CHANGELOG:** "Unreleased" entry for the persistent code index and the Rust API changes.
- **querymt-desktop:** no UI follow-up needed.
