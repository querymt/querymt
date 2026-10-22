# Design

## Context

See proposal.md for motivation. This is a cross-cutting lifecycle/storage change and requires a design.

Original integration points (line references describe the pre-implementation baseline, not the current branch):

- SessionMaterializer creates the database session before constructing SessionRuntime; load prefers persisted cwd but resume currently uses request cwd (`crates/agent/src/agent/session_materializer.rs:258`, `:364`, `:450`). Fork currently copies conversation state through the store (`:515`).
- SessionRuntime owns cwd, MCP services, permission caches, a once-initialized workspace handle, snapshot state, and a per-session execution permit (`crates/agent/src/agent/core.rs:172`). The permit is not a lock between separate sessions sharing a checkout.
- Delegation inherits the parent cwd (`crates/agent/src/delegation/core.rs:842`), while verification and post-delegation hooks use the orchestrator cwd (`:1378`, `:1457`).
- SnapshotBackend already owns track/diff/restore/gc (`crates/agent/src/snapshot/backend.rs:72`). GitSnapshotBackend uses gix and a path-keyed shadow bare repository with core.worktree pointing at the supplied directory (`crates/agent/src/snapshot/git.rs:39`, `:62`). This is a snapshot working directory, not an isolated linked checkout of the user's repository.
- Snapshot capture omits hidden paths and ignored files (`crates/agent/src/snapshot/git.rs:186`). Snapshot restoration is therefore not a full Git baseline export, and its commits do not share the project's history.
- Shell defaults to session cwd but accepts overrides; absolute tool paths are unrestricted (`crates/agent/src/tools/builtins/shell.rs:126`, `crates/agent/src/tools/context.rs:73`). Worktree isolation is not confinement.
- Session views group by cwd (`crates/agent/src/session/sqlite_storage/view_store.rs:576`). The current slash-command types represent prompt/script expansion, not the WorkPacket branch's native plugin API (`crates/agent/src/slash_commands/types.rs:7`).

The user confirmed that isolated delegates must include parent edits. The branch review identified additional correctness gaps; the decisions below incorporate the approved remediation plan. Completed implementation tasks are historical progress, not evidence that these revised contracts have been satisfied.

## Goals / Non-Goals

**Goals:**
- One writable managed checkout per isolated session, including each parallel delegate.
- A single effective workspace binding throughout initialization, execution, verification, undo, and resume.
- Preserve parent working contents without changing its index, HEAD, branch, or files.
- Reuse snapshot infrastructure; introduce only missing linked-checkout, Git-baseline, and ownership functionality.

**Non-Goals:**
- WorkPacket scheduling, live cwd mutation, automatically merging or cherry-picking results, remote filesystem transfer, arbitrary filesystem sandboxing, and automatic retention sweeps.
- Copying ignored files/secrets, provisioning dependencies, or silently flattening dirty submodules into baseline commits.
- Expanding existing undo's file-type/hidden-file coverage as part of worktree support.

## Decisions

### 1. Workspace service and durable binding, not packet metadata

Add a worktree service with an injectable Git operations boundary and a SQLite-backed workspace repository exposed through the existing storage-backend pattern. Keep the service independent of WorkPacket, delegation scheduling, and snapshot implementation.

Persist:
- workspace ID, node-local repository identity derived from canonical Git common-directory location, checkout path, managed branch, original project location;
- source HEAD and immutable baseline commit, baseline mode, optional parent workspace/session/delegation IDs;
- cwd relative to checkout root, lifecycle state, ownership, creation timestamps and last error;
- session-to-workspace association and result references (baseline, result commit if present, verification status);
- replayable provisioning inputs: requested execution/baseline policy, requested ref and resolved source identity when available, source checkout, relative cwd, intended branch/path, operation ID, and allocation phase;
- pending isolated session/fork intent so a persisted conversation cannot become runnable as local execution while workspace creation or binding is incomplete.

Git status is observed, not a persisted lifecycle state: dirty/clean can change outside QueryMT. Use lifecycle states provisioning, ready, failed, missing, archived, removing, removed; retain explicit failure diagnostics. Do not treat a clean worktree as evidence of integration.

Repository identity groups related sessions; checkout identity keys execution/index/snapshot state. Canonical common-dir identity correctly groups linked worktrees without sharing their file indexes. Managed worktrees live under a configurable durable data directory (default `~/.qmt/worktrees/<repo-id>/<workspace-id>`), never the disposable snapshot cache. Do not reuse removed workspace paths, because current snapshot identity is path-derived.

Alternative: adding only cwd fields loses ownership, project grouping, baseline provenance, and recovery semantics. A packet-owned registry also unnecessarily depends on the other branch.

### 2. Resolve policy before session startup

Expose typed creation options: execution mode local or worktree, base mode commit or current-working-tree, optional ref/name. For isolated top-level sessions default to current HEAD, explicitly disclosing excluded local edits; allow current-working-tree as an explicit choice. Existing sessions and omitted options retain local behavior.

Delegation policy inherits from the parent's execution policy: isolated parents isolate all delegates by default (including read-only delegates for a simple, predictable first version). Local parents retain legacy sharing unless configured/requested otherwise. Expose `isolation` on the delegate tool/request and persist the resolved policy. An explicit local override is disclosed as shared execution, never presented as isolated. Policies are enforceable configuration, not model-only prompt instructions.

Create/register the workspace before MCP, index, hooks, or actor initialization. Use the actual worktree-relative cwd everywhere. Resume and load must resolve a persisted association ahead of any caller cwd; conflicting requests fail, and missing worktrees do not fall back. A managed workspace has one session owner; multiple connections to that same session are allowed, a second writable session attachment is not.

Forking an isolated conversation allocates a new checkout from the source's current working contents; historical messages remain context, not a claim that files have rewound to the fork message. Persist the fork's effective execution cwd consistently with its workspace association. All downstream consumers, including delegation, native worktree creation, hooks, and undo child filtering, resolve the binding rather than using a copied source cwd. Historical snapshot entries stay associated with their original workspace and cannot be restored in the new one. No live switch operation is introduced.

Validate fork options before copying conversation state. Persist pending isolation intent with the fork record, then allocate and bind before marking it runnable. Ordinary failures compensate only newly allocated unused resources or retain an explicitly failed/pending record; process interruption leaves a recoverable, non-runnable record. Load/resume, listing, and dispatch must not interpret an unbound pending isolated fork as a legacy local session. Owner binding, execution cwd, and runnable-state publication must be atomic within SQLite or protected by this durable startup barrier.

Alternative: changing cwd after actor creation leaves stale workspace handles, MCP roots, permission caches, and undo state.

### 3. Reuse snapshots for undo; use real Git objects for baselines

Keep SnapshotBackend and its current configured GitSnapshotBackend as the only undo tracking/diff/restoration machinery. Feed it the effective session cwd exactly as today, so separate paths naturally use separate shadow repositories. Preserve current subdirectory-scoped undo behavior rather than silently widening it to the repository root.

Do not create a parallel worktree snapshot engine. Do not use shadow snapshot commits as project baseline or merge commits: they omit content, lack the project's ancestry, and have different retention semantics. Baseline capture is repository state materialization, not a replacement undo journal.

Upgrade gix from 0.80 to 0.90 and replace the argv/output-shaped Git runner with typed, injectable repository operations. Use Repository::add_worktree(), prepare_remove_worktree()/remove_worktree(), worktree enumeration, object/tree/index primitives, and reference transactions. Prefer these high-level APIs over direct gix-worktree add/remove plumbing: the lower layer leaves safety policy to callers. QueryMT retains stricter ignored-file, activity-lease, ownership, and result-preservation checks. Repository operations run off async executor threads where they can block.

Open repositories using the canonical common Git directory and resolved checkout identity; never infer a checkout by taking common_dir.parent(). Support ordinary checkouts, separate Git directories, and non-bare linked checkouts backed by a bare common repository; reject a bare source without a checkout. Keep object/ref administration separate from per-checkout index/worktree access. Explicit repository handles and validated refs must prevent inherited GIT_DIR/GIT_WORK_TREE/GIT_INDEX_FILE from redirecting work.

With default features disabled, explicitly enable sha1 alongside the required basic, attributes, and worktree-mutation features. Update Cargo.lock and audit 0.90 error handling (gix::Result), discovery classification, and the Rust 1.88 minimum against the supported toolchain. Migrate every affected snapshot/discovery call site without changing existing undo coverage. The production snapshot backend already uses gix; its Git CLI fixture commands are not a production dependency and need not be replaced merely for consistency.

Native capture uses private in-memory/index state, Git attributes/filters, and object writers rather than emulating git argv or porcelain output. Do not substitute an unfiltered recursive filesystem copy for index semantics. Test production creation, capture, review, recovery, and removal with the Git executable unavailable, creating fixtures beforehand or natively. This removes the Git executable requirement for built-in repository operations, not arbitrary shell tools or configured external filter processes. Both backends can share genuinely identical path/error helpers, but snapshot history and repository administration remain separate boundaries.

References: https://docs.rs/gix/0.90.0/gix/ and https://docs.rs/gix-worktree/0.59.0/gix_worktree/.

### 4. Stable private baselines include parent edits

For current-working-tree capture, hold a checkout-scoped mutation gate for cooperating QueryMT writers while capturing. Acquire it only around workspace/file mutation operations, undo/redo, and baseline capture, not for an entire waiting parent turn. Use a process-shared advisory lock keyed by canonical checkout so multiple QueryMT processes cooperate. Avoid holding a parent gate while awaiting a child: capture and release before launch.

Build a private index seeded from source HEAD, reconcile tracked membership from both HEAD and the real index with actual working-file contents (including deletions and tracked hidden/ignored files), and include non-ignored untracked files. Newly staged files, including ignored files explicitly force-added to the real index but absent from HEAD, remain tracked for capture and use their working contents. Do not force-include genuinely untracked ignored files. Respect supported Git attributes/filter semantics when writing objects. Preserve Git-supported executable bits, symlinks, binary contents, and tracked hidden paths. The parent's staging partition is not propagated: a partially staged file uses its working content, while the real index remains byte-for-byte unchanged. Ignored untracked files are excluded and reported, not silently copied.

Write an immutable baseline tree and, when it differs from HEAD, a private baseline commit parented by source HEAD under a managed baseline ref. Never run commit, stash, reset, or add against the real parent index. Private commits use an explicit internal identity and do not require user commit identity or invoke user commit hooks. If the tree equals HEAD, reuse HEAD. Child branches start from this baseline. Baseline refs remain reachable while any workspace/result depends on them.

Validate source HEAD/index and a content manifest before/after capture. External editors do not honor QueryMT locks; detect observed changes and retry a bounded number of times, then fail explicitly rather than launching from a known inconsistent capture. This is not a transactional filesystem snapshot against arbitrary hostile writers; document that limitation. Delegates dispatched as one parallel batch use one captured baseline; independently requested delegates capture the then-current parent state.

Reject unresolved index conflicts, unborn HEAD, unsupported sparse-checkout/LFS/submodule capture cases in the first version with actionable errors before launch; never report a successful full baseline after dropping unsupported content. Initial baseline support targets ordinary non-bare repositories and linked worktrees; tests must cover supported symlinks and tracked dotfiles. Submodule-bearing and LFS-managed repositories remain explicitly unsupported initially rather than silently returning incomplete checkouts. Resolve the actual checkout root before capture and validation, regardless of a session's relative cwd; preserve that relative cwd only for child execution. Detect submodules using HEAD/index gitlinks as well as repository metadata. Resolve effective attributes using Git attribute semantics, including nested .gitattributes, Git-directory attributes, and macros, rather than matching text in one root file. Unsupported-feature diagnostics identify the feature/path and occur before execution.

Alternative: shadow undo snapshots cannot provide this completeness. Requiring a clean parent was rejected by the user's explicit choice. Auto-committing or stashing the parent's checkout would disturb ongoing work.

### 5. Delegate execution and results are workspace-aware

Allocate the child's workspace before create_delegation_session; retain source parent identity separately from the effective child execution path. Resolve child MCP roots and tool context in that workspace. Run verification there, not at orchestrator cwd. Execution-related hook payloads identify both parent and child workspace; pre-allocation policy hooks keep parent context explicitly rather than claiming a nonexistent child cwd.

The workspace must be on the host that executes filesystem tools. First version supports local filesystem execution, including remote model inference that leaves tools local. Requests to isolate filesystem execution on another node return an unsupported error before dispatch; never send a local path as though it existed remotely.

Resolve a session-scoped slash-command registry from the effective workspace for both catalog discovery and actual prompt/script expansion. Prompt includes and scripts must resolve from that registry's workspace sources, not the process-global original-checkout registry; preserve global-command policy and native /worktree interception.

Editor language bridges or fixed-root MCP services must explicitly support the effective root or be disabled/rejected with a diagnostic. Do not silently query the original checkout. Prompts and events expose the actual execution path, excluded local environment files, and the fact that worktrees are not sandboxes. Absolute paths in old context are not automatically rewritten.

On completion, retain the workspace and report workspace ID/path, branch, baseline OID, current HEAD, changed files relative to the baseline, verification outcome, and dirty/result status. For uncommitted results, capture a private immutable result using the same repository capture primitive; do not stage or commit the child's real index. Cancellation/failure retains recoverable files even if result capture fails. No completion, timeout, or connection close triggers integration or deletion.

Use the same result publication/review service for top-level workspaces and delegates. An explicit result operation can publish an immutable current working-state result for a top-level workspace; inspecting an existing result must not silently replace it with live state. Return retained baseline/result OIDs and refs, baseline-relative changed paths with change kinds, verification status, captured HEAD, separately labeled live HEAD/dirty state, and capture errors. Later edits must not change an already-published delta; a subsequent publication gets a distinct create-only ref. Delegate completion and summarizer output include the review data or a durable reference to it, not just opaque commit IDs.

Dirty-parent baselines are already committed privately. A raw merge of a child branch into the parent's branch could unintentionally bring baseline edits with it. Review therefore presents only baseline-to-result changes, labels private branches accordingly, and explains that integration must apply that delta deliberately (or commit the baseline coherently first). A supported automatic apply/merge transaction is deferred, rather than pretending raw branch merge is always correct.

### 6. Management surface and conservative cleanup

Provide typed create/list/status/result/remove agent operations, with ACP extension metadata/operations rather than changes to upstream protocol types. Add an intercepted native `/worktree` command family backed by the same service; do not implement management as model-generated shell commands or depend on a WorkPacketSlashPlugin absent from this branch.

- `create [ref] [name]` creates an isolated session and returns its identity/path; it does not move the current actor. An explicit include-local-changes option selects current-working-tree capture.
- `list` and `status [id]` expose owner, project, path, branch, lifecycle, live Git state, and baseline mode.
- `result [id]` exposes baseline-relative review/provenance and verification status.
- `archive <id>` retires inactive execution without deleting history or the durable owner binding. It is permanent: load/resume is rejected thereafter. Runtime-held shared checkout activity leases and exclusive archival/removal leases distinguish active execution from durable ownership across processes.
- `recover <id>` completes interrupted provisioning/removal intent without launching an actor or binding a new owner.
- `remove <id>` refuses non-archived owners, active activity leases, dirty/untracked/ignored content, and unintegrated result commits without retained recovery references. First version has no force-delete operation. Committed results can be preserved by retaining their branch/ref; report exactly what remains.

Removal of a clean baseline-only inactive checkout is allowed with explicit user intent, retaining any baseline refs still referenced by records. For changed commits require proof they are preserved in a retained result ref or an accepted target history, not merely a clean index. Never delete externally owned worktrees. Observe branch targets during preservation checks and delete branches only if their targets are unchanged, using Repository::delete_local_branches_if_unchanged() or an equivalent expected-target transaction. Repository-admin locks only coordinate QueryMT; ordinary Git processes may update refs. On a changed target, retain the branch and report the race or refuse/retry after revalidation, never unconditionally delete it. Repeat this rule during interrupted-removal recovery. Session listing uses repository/project identity for associated sessions while preserving legacy grouping for unassociated local sessions.

### 7. Recovery, concurrency, and snapshot provenance

Worktree creation and SQLite writes cannot form one atomic transaction. Persist complete replayable provisioning intent with unique operation/workspace IDs before capture, retain baseline identity/ref publication progress, allocate branch/worktree, persist ready binding and execution cwd, then start execution. Recovery handles the actual initial baseline-absent intent rather than requiring a pre-populated baseline. If no immutable capture was durably bound, an explicit recovery may retry a newly validated capture from the recorded source and disclose that it reflects recovery-time contents; if the source is unavailable, retain a non-runnable diagnostic state. Once a baseline is bound, never silently recapture or change it. Idempotently ensure its retained baseline ref exists and has the expected target before publishing Ready; reject conflicting existing refs. Compensate only newly allocated, unused resources; after a crash reconcile intent with Git worktree inventory and actual paths. Never delete an unknown existing directory. Serialize repository-admin mutations across processes using a common-dir keyed lock, separate from per-checkout capture gates. Persist Removing intent before deleting any Git resources; ready-only owner attachment and an atomic unowned-or-archived removal transition prevent ownership races. Recovery rechecks safety and completes Removing idempotently.

The current undo implementation scans child-session patches and restores them through the caller's worktree (`crates/agent/src/agent/undo.rs:126`, `crates/agent/src/agent/undo.rs:217`). Retain child aggregation only for children demonstrably sharing the same snapshot workspace/cwd; exclude isolated children from parent undo aggregation. Parent undo must not fail merely because isolated children exist, nor restore their patches into the parent. Validate all selected patches before any restoration, including the fallback snapshot path, rather than warning and continuing after a provenance mismatch.

Attach workspace identity to new snapshot/revert provenance (or equivalent durable association) and validate it before undo. Legacy snapshots remain usable in the original local session, but copied parent snapshots in a newly isolated fork are context-only. Resume of the same workspace keeps its existing snapshot path and history. Before removing a managed workspace, require its owner to be inactive; retained snapshot metadata is not justification to delete unsaved files. Undo modifies files through the existing backend, not project branch refs or HEAD.

### As-built: native repository operations (section 10)

Implemented as specified: `crates/agent/src/worktree/repo_ops.rs` replaces the argv/stdout Git runner with a typed `RepositoryOps` boundary (`GixRepoOps`), implemented entirely with gix 0.90 primitives (reference edits, `add_worktree`/`remove_worktree`/worktree enumeration, status platform, private index/tree/commit capture, commit-graph walks). The CLI adapter (`git.rs`) is compiled for test fixtures only (`#[cfg(test)]`); no production path spawns a Git executable. Every blocking gix call runs through `tokio::task::spawn_blocking`, never on an async executor thread. Repositories are opened with `gix::open::Options::isolated()`, so inherited `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE` cannot redirect operations (verified by test). Status comparisons keep object access on the calling thread (staged changes are computed by flattening the HEAD tree synchronously) to avoid sharing the thread-local object store with gix's status worker threads. Fault injection happens at named operation boundaries via `FaultInjectingRepoOps`/`FailurePoint`; tests never parse Git commands or fabricate porcelain output. Toolchain: gix 0.90 with `default-features = false` and `sha1` plus `basic`, `attributes`, `worktree-mutation`, `status` (Rust ≥ 1.88).

### As-built: gix 0.90 upgrade (section 20)

Bumped to gix 0.90 / gix-worktree 0.59. No API migration was needed for the call sites we use. Behaviour changes handled in `repo_ops.rs`:

- `add_worktree` now copies the source worktree's sparse-checkout state. We administer through the common directory, i.e. the main worktree, so sparse main checkouts would yield sparse children. Creation is refused up front (`core.sparseCheckout` or `info/sparse-checkout`) and a sparse child is rolled back with `Force::Never`. This is a deliberate regression for sparse monorepos in commit mode; sparse support is a follow-up.
- Replacement refs are enabled by default and cannot be disabled via the environment under `isolated()`. All handles now set `core.useReplaceRefs=false` so preservation/ancestry checks see real objects.
- `worktree.useRelativePaths` would permanently upgrade the shared repository format; creation is refused unless `extensions.relativeWorktrees` is already set.
- Not adopted (follow-up): locking managed worktrees via `Proxy::set_lock`. gix 0.90's `Force` has no keep-changes lock override (`OverrideLock` discards changes), so removal would need unlock -> `Force::Never` -> relock-on-failure, plus prune/recovery changes. Two-phase `prepare_add_worktree` is likewise deferred.

## Risks / Trade-offs

- [Disk usage from worktrees and private commits] -> Durable retention, visible status, explicit conservative removal; no surprise cache eviction.
- [External writers during capture] -> Cooperative gates plus validation/retry; document the remaining non-atomic external-writer limitation.
- [Baseline files can contain secrets even when not ignored] -> Capture only declared repository working state, never push private refs automatically, warn that private baselines enter the local object database.
- [Unsupported Git layouts] -> Fail closed before child launch with explicit diagnostics; add coverage before claiming support.
- [Result integration is manual initially] -> Supply an immutable baseline-relative result and clear recovery paths; defer unsafe automatic merging.
- [Existing undo omits some file classes] -> Preserve and document its semantics; regression-test supported paths and Git metadata exclusion rather than claiming complete Git rollback.
- [Concurrent ACP/session-update work] -> Keep integration additive, coordinate event payload changes during implementation, and do not overwrite the other change's dispatch/lifecycle logic.
- [Worktree is not a sandbox] -> Surface effective roots and reject mismatched integrations; OS-level confinement is a separate feature.

## Migration Plan

1. Upgrade and check the native gix dependency and typed operations boundary, preserving snapshot behavior; add migration/regression coverage before replacing the CLI implementation.
2. Add nullable workspace associations and new registry/provenance tables through existing SQLite migrations. Existing sessions stay local; no checkout is moved.
3. Add disabled-by-default top-level isolation configuration and explicit creation/delegation options. Enabling isolation for a session changes the inherited delegate default to isolated execution.
4. Ship local lifecycle, baseline capture, session routing, undo compatibility, and delegate results together; do not advertise isolation before all workspace consumers resolve correctly.
5. Validate existing databases and old snapshot histories, then test crash recovery and concurrent sessions in temporary real Git repositories.
6. Roll back behavior by disabling new isolated creation while preserving worktrees, private refs, and registry data for manual recovery. Do not drop tables or delete workspaces as part of rollback; older binaries must not be used to resume isolated sessions without understanding their persisted execution paths.
