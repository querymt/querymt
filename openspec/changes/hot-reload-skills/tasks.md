## 1. Strict and transactional discovery

- [ ] 1.1 Refactor `crates/agent/src/skills/discovery.rs` to expose a strict discovery path for refresh that returns an error when any selected source cannot be traversed, while preserving missing-directory-as-empty behavior and malformed-entry diagnostics/skipping. Keep existing best-effort startup behavior unless callers can be migrated without changing semantics. Verify: discovery tests cover unreadable traversal, a missing source, and mixed valid/malformed entries.
- [ ] 1.2 Add `SkillRegistry::reload_from_sources(&mut self, sources, include_external) -> Result<usize>` in `crates/agent/src/skills/registry.rs`: build both replacement maps from strict discovery results and swap them in only after complete success; retain the old registry unchanged on error. Verify: `cargo test -p querymt-agent skills::registry` covers additions, removals from both indexes, edits, and failed-discovery preservation.

## 2. Consistent callable skill identity

- [ ] 2.1 Key registry registration, lookup, sorted names, and tool listing by `SkillMetadata::effective_id()`; when the display name differs, keep it as descriptive metadata without creating a callable alias. Verify: registry tests cover an explicit protocol `id` different from `name`, source precedence, and removal/reload by effective ID.
- [ ] 2.2 Confirm permission checks and not-found output use callable effective IDs. Verify: tool tests deny a protocol skill by explicit ID and demonstrate that its differing display name is not an invocation alias.

## 3. Tool retains discovery config and refreshes at schema snapshots

- [ ] 3.1 Extend `SkillTool` (`crates/agent/src/skills/tool.rs`) with `sources: Vec<SkillSource>` and `include_external: bool`, update `SkillTool::new`, and pass cloned values from `build_skill_tool` (`crates/agent/src/skills/mod.rs`). Add private `refresh_registry()` that locks the registry, calls transactional reload, logs source-context errors, and preserves old contents. Verify: `cargo build -p querymt-agent` and existing `skills::tool` tests pass.
- [ ] 3.2 Call `refresh_registry()` at the start of `SkillTool::definition()` before obtaining one coherent description/enum snapshot under the registry lock. Verify: tool tests add, edit, and remove skill directories between successive `definition()` calls and assert matching descriptions and enums.
- [ ] 3.3 Add a concurrency regression test that invokes `definition()` and `call()` from separate workers and asserts each observed registry/schema state is complete and internally consistent, with no poisoned-lock failure. Verify: the focused test passes repeatedly.

## 4. Reload-on-miss invocation

- [ ] 4.1 In `SkillTool::call()` (`crates/agent/src/skills/tool.rs`), check the registry by callable ID and, only on a miss, refresh once and retry. If still absent, return a deterministic error naming the requested ID and sorted available IDs; state explicitly when none are available. Verify: tests cover a skill added after the last schema snapshot, a removed skill, an empty registry, and a refresh failure that preserves and reports the old snapshot.
- [ ] 4.2 Ensure existing registered skills are not re-read on every invocation; content edits become visible after the next `definition()` refresh. Verify: a test edits an existing skill, confirms the pre-refresh call still uses the snapshot, then confirms `definition()` and the following call use updated metadata/content.

## 5. Scenario and integration coverage

- [ ] 5.1 Test disabled protocol skills remain absent after refresh and duplicate/source-precedence behavior remains unchanged. Verify: focused discovery, registry, and tool tests pass.
- [ ] 5.2 Test permissions survive refresh for allow, ask, and deny rules keyed by callable effective ID, without prompting denied or missing aliases. Verify: focused permission/tool tests pass.
- [ ] 5.3 Test failed-refresh atomicity with a deterministic injected or synthetic traversal error rather than platform permission bits alone, so the test remains reliable under privileged CI users. Verify: prior skills remain listed and loadable and no healthy-source partial update leaks into the registry.
- [ ] 5.4 Add an integration-level test through `build_skill_tool` or `ToolRegistry::definitions()` proving schema collection refreshes the skill tool for standalone construction; confirm shared builder coverage is sufficient for quorum delegate/planner construction or add a focused quorum test. Verify: relevant integration tests pass.

## 6. Full verification

- [ ] 6.1 Run formatting and focused checks: `cargo fmt --all -- --check`, `cargo test -p querymt-agent skills::`, and `cargo clippy -p querymt-agent --all-targets --all-features -- -D warnings`. Verify: no failures or warnings introduced.
- [ ] 6.2 Run the full agent test suite: `cargo test -p querymt-agent`. Verify: no regressions.
- [ ] 6.3 Run `openspec validate hot-reload-skills --type change --strict --no-interactive`. Verify: proposal, specification, design, and tasks pass strict validation.
- [ ] 6.4 Manual end-to-end check: start a session in this repo, add a temporary skill with an explicit ID different from its display name, confirm the next model request advertises and loads it by ID, edit it and confirm the following request sees the update, then remove it and confirm it disappears and stale invocation returns the deterministic availability error. Verify: observed behavior matches the scenarios and remove the temporary skill afterwards.
