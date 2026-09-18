## Context

`crates/agent/src/skills/` implements lazy skill loading: `build_skill_tool` (mod.rs) discovers skills from global/project sources once at agent construction, populating a `SkillRegistry` held by `SkillTool` behind `Arc<Mutex<..>>`. Three constraints shape refresh:

- `AgentConfig::collect_tools` (agent/agent_config.rs) rebuilds the model-facing tool list for model requests by calling `Tool::definition()`, and definitions are also collected by tests and exports. Refreshing in `definition()` therefore guarantees freshness before a model request but is not literally limited to once per turn.
- `SkillRegistry::load_from_sources` currently merges into its `by_name`/`by_path` maps and never removes entries, and `build_skill_tool` discards the computed search paths after the initial load, so neither removals nor re-discovery are possible today.
- `discover_all` logs and suppresses each source traversal error before returning `Ok` with partial results. A replace operation cannot treat that result as a complete snapshot without risking accidental deletion. Discovery deduplicates protocol entries by `SkillMetadata::effective_id()`, while `SkillRegistry::register` currently keys them by `metadata.name`; refresh must also resolve that identity mismatch.

## Goals / Non-Goals

**Goals:**

- Skill set advertised to the model tracks the filesystem whenever a model-facing schema snapshot is generated (add, remove, edit).
- Skills added after the last schema snapshot are loadable through reload-on-miss.
- Refresh publishes complete snapshots atomically and never replaces known skills with partial discovery results.
- Skill identity is consistent from discovery through advertisement and invocation.
- Zero changes to discovery sources, precedence, `.agents/skills` parsing rules, or permission semantics.

**Non-Goals:**

- File-watcher or actor-based (kameo) live refresh — can be layered later behind the same refresh entry point.
- Slash-command hot-reload — `SlashCommandRegistry::reload` exists but has no production wiring in this workspace.
- Caching or incremental discovery (mtime-based short-circuit) — deferred until per-turn cost is measured as a real problem.
- Changes to remote skill support (`skills/remote.rs` is unimplemented stubs).

## Decisions

### D1: Refresh at `definition()` (schema snapshot) instead of a `notify` watcher or kameo actor

`definition()` is invoked while `collect_tools` constructs model-facing schemas, which is exactly when freshness matters; it may also be invoked by tests, exports, or other inspection paths. The guarantee is therefore refresh-before-schema-snapshot, not exactly-once-per-turn. Discovery is a walk of a handful of directories plus parsing small markdown files, so this call boundary is a natural debounce.

- *Alternatives considered*: (a) `notify::RecommendedWatcher` + kameo `SkillActor` mirroring `WorkspaceIndexActor`/`FileIndexWatcher` — rejected for now: it must handle watching not-yet-existing source dirs (e.g. a deleted `.qmt/skills`), per-instance lifecycle for the quorum delegate/planner tools, and an actor that duplicates serialization the existing `Mutex` already provides. The code index needs actors because it maintains *incremental* indexes; skills are cheap to rediscover wholesale. (b) Reload only on `call()` miss — insufficient: the listing/enum would never refresh, the primary complaint.

### D2: Add a strict discovery path for transactional refresh

Refactor discovery so refresh can distinguish a complete scan from source traversal failure. The existing best-effort `discover_all` behavior may remain for compatibility and initial startup, but `reload_from_sources` uses a strict variant that returns `Err` if any selected source walk fails. Nonexistent source directories continue to produce an empty result, and malformed individual skill files continue to be logged and skipped so one bad entry does not freeze all valid updates.

- *Alternative considered*: infer failure from logs or compare result counts — rejected because neither proves completeness.
- *Alternative considered*: make all discovery strict — unnecessary behavioral expansion for existing startup callers; a dedicated strict path limits the change.

### D3: Replace registry state only after complete discovery

`reload_from_sources(&mut self, sources, include_external)` builds replacement `by_name` and `by_path` maps off to the side from strict discovery results, then swaps both maps into the registry only after all discovery and registration work succeeds. The old merge-only `load_from_sources` remains available for existing callers.

- *Why build then swap instead of clear then insert*: readers protected by the mutex still receive one coherent state, and future fallible registration cannot leave a partially populated registry.
- *Why both maps*: `by_path` is a lookup index; retaining stale paths would alias deleted directories.

### D4: Registry keys use callable effective IDs

`SkillRegistry::register` keys `by_name` by `skill.metadata.effective_id()` rather than the display name. `names()`, tool schema enums, lookup, duplicate replacement, permission checks, and not-found errors therefore use one callable identity. `list_for_description()` renders the callable ID and description, optionally including the human-readable name when it differs so the schema remains understandable.

- *Alternative considered*: register both ID and name as aliases — rejected because it creates ambiguous permission and override behavior and advertises aliases not defined by the protocol.

### D5: `SkillTool` retains its discovery configuration

`SkillTool::new` additionally stores `sources: Vec<SkillSource>` + `include_external: bool`; `build_skill_tool` passes them through. A private `refresh_registry()` helper locks the mutex and reloads, logging a warning on failure and keeping prior contents.

- *Alternative considered*: re-deriving sources from config at refresh time — rejected: `build_skill_tool` already resolves custom/configured paths and per-builder project roots (quorum paths construct skill tools with different roots); re-deriving would duplicate that resolution and risk divergence.

### D6: Reload-on-miss in `call()` with an actionable error

After validating the requested callable ID and applying its name-keyed permission rule, `call()` checks the registry. On a miss, it refreshes once and retries; if refresh fails, it logs the discovery failure and returns the normal not-found response from the preserved snapshot. If still missing, the error names the requested ID and deterministically enumerates registered callable IDs, or explicitly says no skills are available. This covers additions after the last schema snapshot and turns stale-enum confusion into a self-explanatory failure.

### D7: Permissions stay callable-ID-keyed config

`SkillPermissions` is constructed from config independently of discovery. Permission checks use the same effective ID accepted by `call()`, so re-discovered skills inherit their rules with no refresh-time mutation; tests cover an explicit protocol ID that differs from the display name.

## Risks / Trade-offs

- [Refresh cost on every schema snapshot] → Measure with a focused benchmark or timing assertion only if existing tests reveal a regression; if source trees grow unreasonably, add an mtime-based short-circuit behind `refresh_registry` later.
- [`definition()` called from non-turn contexts (tests, exports)] → Refresh is idempotent; those callers receive a current snapshot rather than relying on a once-per-turn side effect.
- [One unreadable selected source blocks updates from healthy sources] → Preserve the last complete snapshot and log the failing source; atomic correctness is preferred over publishing a misleading partial set.
- [Mutex contention between refresh and concurrent `call()`] → Discovery currently occurs while holding the registry mutex, prioritizing a coherent snapshot; add a concurrency regression test and move discovery outside the lock only if contention becomes measurable.
- [Listing lags after its schema is generated] → Accepted by design; D6 ensures invocation of a newly added skill can still succeed.
- [Stale skills in long-lived processes that never build a new turn] → Out of scope: refresh triggers on turn boundaries by design.

## Migration Plan

No data or config migration; no new dependencies. `SkillTool::new` gains parameters but is constructed only inside `crates/agent` (via `build_skill_tool` and tests). Existing protocol skills whose explicit `id` differs from `name` become callable by the documented stable ID; configurations should key permissions by that callable ID. Rollback is a straight revert.

## Open Questions

None.
