# Tasks

## 1. File-Effect Contract

- [ ] 1.1 Add normalized post-tool file-effect request and wire-schema types, including explicit unavailable versus empty states, and verify serialization/schema fixture tests cover added, modified, removed, empty, and null payloads
- [ ] 1.2 Add deterministic workspace-relative path normalization, sorting, deduplication, and change-set limits, and verify unit tests cover duplicate paths, mixed effect kinds, and truncation metadata

## 2. Reconciliation Pipeline

- [ ] 2.1 Refactor pre/post Merkle scanning into a reusable reconciliation result independent of snapshot persistence, and verify existing snapshot and diff tests remain green
- [ ] 2.2 Determine tracking demand from snapshot, turn-diff, and matching PostToolUse consumers before tool execution, and verify read-only calls skip scans while unknown mutating tools are conservatively tracked
- [ ] 2.3 Move post-tool reconciliation before `PostToolUse`, reuse its result for snapshots and turn diffs, and verify an ordering test observes reconcile -> hook -> truncate -> ToolCallEnd -> persistence

## 3. Hook Delivery

- [ ] 3.1 Populate internal and command-hook PostToolUse inputs with normalized file effects, and verify a command-hook fixture receives a shell-created file without relying on tool arguments
- [ ] 3.2 Preserve legacy PostToolUse behavior when handlers ignore the new field, and verify existing hook integration tests pass unchanged
- [ ] 3.3 Cover unavailable roots, empty diffs, add/modify/remove effects, and overlapping background changes in integration tests, verifying the contract reports interval effects without claiming process attribution

## 4. Verification and Documentation

- [ ] 4.1 Regenerate committed hook JSON schema fixtures and verify schema drift checks pass
- [ ] 4.2 Document PostToolUse ordering, payload semantics, limits, and snapshot-policy independence, and verify examples contain both an empty and non-empty effect set
- [ ] 4.3 Run the agent crate's hook, snapshot, index, and tool-execution test suites plus formatting and lint checks, and record any unrelated failures
