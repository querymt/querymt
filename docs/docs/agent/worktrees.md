# Git Worktree Workspaces

QueryMT can run sessions inside **managed Git worktree workspaces**: durable,
isolated checkouts of a repository that live under QueryMT's storage root
instead of your project directory. Multiple sessions — and parallel delegated
sub-agents — can then work on the same project without sharing writable
files.

!!! warning "Isolation is not sandboxing"
    A managed workspace is a separate *checkout*, not a security boundary.
    Tools still execute with your user's permissions and can still touch
    absolute paths outside the workspace. Isolation prevents *accidental*
    file collisions between sessions; it does not confine a misbehaving
    tool.

## Enabling the workspace service

Workspace isolation is opt-in, and **enabling the service never isolates a
session by itself**. The service only makes isolation *available*; a session
is isolated only when it asks for it (see
[Creating an isolated session](#creating-an-isolated-session)). An option-less
`session/new`, `chat_session()` or Python `chat_session()` stays local, creates
no worktree, and is unchanged whether or not the service is enabled.

=== "TOML"

    ```toml
    [agent.workspace]            # or [quorum.workspace] for a multi-agent quorum
    enabled = true               # default false: no service, behavior unchanged
    # storage_root = "~/.qmt/worktrees"   # default; relative paths resolve
    #                                       # against the config file
    # baseline = "commit"        # "commit" (default) | "current_working_tree"
    ```

=== "Rust"

    ```rust
    use querymt_agent::api::Agent;
    use querymt_agent::config::WorkspaceSettings;

    let agent = Agent::single()
        .provider("anthropic", "claude-sonnet-4-5-20250929")
        .cwd("/path/to/repo")
        .workspace(WorkspaceSettings::enabled())
        .build()
        .await?;
    ```

    A prebuilt service can be injected instead with `.workspace_service(arc)`
    (also on `Agent::multi()`); it takes precedence over the settings. Lower
    level embedders can still attach one with
    `AgentConfigBuilder::with_workspace_service`.

=== "Python"

    ```python
    agent = await Agent.single(
        provider="anthropic", model="claude-sonnet-4-5-20250929",
        cwd="/path/to/repo",
        worktrees=True,   # or {"storage_root": "...", "baseline": "commit"}
    )
    ```

    `Agent.from_config(path)` picks the section up from TOML. Unknown keys in
    the dict raise `ValueError`.

Settings are validated **when the agent is built**, not at the first isolated
session: the storage backend must expose a workspace repository, `storage_root`
must be creatable and be on the local filesystem, and a quorum delegate
configured `isolation = "isolated"` requires the service. A quorum builds
exactly **one** service shared by the planner and every delegate.

Managed checkouts live under `<storage-root>/<repo-slug>/<workspace-id>/`,
survive session completion, process restarts, and machine reboots. They
accumulate until you archive and remove them, so programmatic callers creating
sessions in a loop should do so deliberately (see
[Management from code](#management-from-code)).

## Creating an isolated session

Isolation is requested per session through the ACP `_meta.querymt.sessionWorkspace`
extension (additive; upstream protocol types are unchanged):

```json
{
  "cwd": "/home/me/project",
  "_meta": {
    "querymt": {
      "sessionWorkspace": { "execution": "worktree" }
    }
  }
}
```

| Option | Values | Default | Meaning |
|--------|--------|---------|---------|
| `execution` | `local`, `worktree` | `local` | `worktree` allocates a fresh checkout before the session starts |
| `baseline` | `commit`, `current_working_tree` | `commit` | `commit` starts from HEAD (or `baseRef`); `current_working_tree` also captures uncommitted edits and non-ignored untracked files |
| `includeLocalChanges` | boolean | `false` | shorthand for `"baseline": "current_working_tree"` |
| `baseRef` / `base_ref` | Git ref | — | branch the new workspace from an explicit ref |

Semantics that matter:

- **Omitted options keep local behavior.** No managed worktree is created.
- **The actual working directory is disclosed.** The session materializes
  with `cwd` set to the bound checkout — e.g.
  `~/.local/share/qmt/worktrees/<repo-slug>/ws_01ABCdef…` — and that path, not
  the source directory, is what file tools, the shell, indexes, prompts, and
  hooks use. Ignored (gitignored) untracked files are never copied; the
  response discloses that exclusion for current-working-tree baselines.
- **The source checkout is untouched.** HEAD, branch, index, stash, and
  working files of the original directory are never modified by creation.
- **Requested isolation never falls back.** If Git, the repository, durable
  storage, or local filesystem placement is unavailable, creation fails with
  an actionable error instead of silently running shared.

### Default baseline

`baseline` in the settings is a *default for explicitly requested top-level
isolation* that did not choose a baseline itself. It never decides **whether**
to isolate. Precedence, per request:

1. an explicit `baseline` / `includeLocalChanges` value (including an explicit
   `false`) always wins;
2. an explicit `baseRef` means a commit baseline from that ref, so a
   `current_working_tree` default yields to it instead of failing validation;
3. otherwise the configured default applies.

The resolved mode is disclosed in the `session/new` response and in `create`
results (`baseline`, `localChangesExcluded`). Delegates always start from the
parent's working contents and forks keep their own rules; neither is affected.

`current_working_tree` capture details:

- **Tracked membership comes from HEAD plus the real index.** A file
  force-added to the index is captured with its working contents — even when
  ignore rules would exclude it — and later working edits to it are captured
  too. Genuinely untracked ignored files stay excluded and disclosed. The
  parent's staging partition is not imported: a partially staged file uses
  its working content, and the real index stays byte-for-byte unchanged.
- **Capture is repository-wide.** A session started in a subdirectory still
  captures from the resolved checkout root; the relative cwd only applies to
  child execution. Root-level `core.sparseCheckout` therefore cannot be
  bypassed from a subdirectory.
- **Unsupported content is rejected with path diagnostics before any child
  launches.** Active submodules are detected from gitlink entries in HEAD or
  the index (no `.gitmodules` required); sparse checkouts and unborn HEAD or
  unresolved conflicts fail as before.
- **Sparse repositories are rejected in commit mode too.** `gix` 0.90 copies
  the sparse-checkout patterns of the main worktree into every new linked
  worktree, so a sparse main checkout would silently produce sparse children
  whose absent paths look like deletions. Creation fails before anything is
  made when `core.sparseCheckout` is enabled or `info/sparse-checkout` exists,
  and a child that nevertheless turns out sparse is rolled back. Supporting
  sparse checkouts is a possible follow-up.
- **`worktree.useRelativePaths` is refused until the repository already uses
  `extensions.relativeWorktrees`.** Otherwise `gix` would permanently upgrade
  the shared repository format (Git 2.48+ only), even if creation then fails.
- **External filter processes are unsupported and rejected via effective
  attributes.** Attribute resolution follows Git semantics — nested
  `.gitattributes` files, `$GIT_DIR/info/attributes`, global attribute
  files, and `[attr]` macros — and any path resolving to `filter=lfs` (or
  any other external clean/smudge filter) fails capture with a diagnostic
  naming the path. A filter configuration with no matching paths is not a
  blocker: rejection is path-based, never a root-file text scan. Working
  contents are captured as raw bytes; text/eol normalization and external
  filters are not applied.
- **Externally changing sources are retried, then rejected.** The source
  HEAD, index (including newly staged files), and captured tree are
  validated before and after capture; a mismatch retriggers a bounded retry
  and fails explicitly rather than launching from a known-inconsistent
  baseline. This is not a transactional snapshot against arbitrary hostile
  writers — a write landing after the post-validation is undetectable.

## Native Git operations

All built-in workspace operations — baseline capture, branch and ref
administration, linked-worktree creation/inventory/removal, status, and
ancestry checks — are implemented natively in Rust on top of `gix` (0.90).
No Git executable is required, installed, or invoked for these built-in
operations; QueryMT needs a supported toolchain (Rust ≥ 1.88) but nothing
else. Building the `WorkspaceService` takes a `GixRepoOps` value (see the
example above); there is no Git-CLI configuration knob.

Replacement refs (`refs/replace/*`, grafts) are ignored by every handle
(`core.useReplaceRefs=false`): `gix` enables them by default and `isolated()`
cannot switch them off through the environment, and a replacement could make
unpreserved commits look reachable from a kept ref during removal checks.

Repository-opening policy: administration operations (branches, refs,
worktree registration/inventory/removal, ancestry) open the canonical Git
**common directory** resolved from the source at creation time and stored on
every workspace record — never a working directory guessed from a checkout.
Per-checkout operations (status, HEAD, capture, index reads) open the explicit
checkout path. Both use an isolated open that ignores inherited `GIT_DIR`,
`GIT_WORK_TREE`, and `GIT_INDEX_FILE` overrides, so an ambient environment
cannot redirect operations to a different repository, worktree, or index.
Repository-local configuration is still honored.

### Supported repository layouts

Workspace creation, status, capture, result review, recovery, and removal
work for all of these source layouts:

- **Ordinary checkout** — `.git` directory directly below the working tree.
- **Separate Git directory** — the checkout carries a `.git` *file* pointing
  at a Git directory stored elsewhere (the `git init --separate-git-dir`
  layout, with `core.worktree` pointing back). Administration targets the
  external Git directory; the checkout is never assumed to contain one.
- **Existing linked source** — the session starts in a linked worktree of an
  ordinary repository. Its managed checkout becomes a sibling linked worktree
  of the same shared repository.
- **Linked checkout backed by a bare common repository** — the source is a
  linked worktree whose common repository is bare (e.g. `project.git`). The
  bare registration itself has no working tree and is never misread as a
  usable checkout; the linked checkout is the only one listed and managed.

A bare source without a working checkout is rejected explicitly with
`BareRepositoryUnsupported` before any workspace resources are allocated.
Other layout diagnostics: `NotGitRepository` (no repository at or above the
path), `RelativeCwdOutsideCheckout`, and `RelativeCwdAbsent` (a subdirectory
session whose directory the selected baseline lacks).

All paths are canonicalized at resolution time, so path aliases (symlinks)
resolve to identical repository and checkout identities: a durable workspace
has exactly one owner and one administration lock no matter which alias a
caller uses, and project grouping keys on the canonical common directory.
Grouped sessions share the project identity but keep distinct execution
paths and per-checkout indexes — grouping never merges execution state.

This native layer is distinct from the Git activity a session may perform
through its own tools: arbitrary shell commands the model runs, and any
configured external filters or hooks, execute Git however they choose and
are neither intercepted nor restricted by this service. Built-in typed
operations never shell out and never parse Git command output.

## Discovering support

An agent with a workspace service advertises it in `initialize`
(`agentCapabilities._meta.querymt.workspace`), so clients can offer an explicit
Local / Worktree choice at creation. Agents without the service omit it.

```json
{ "supported": true, "defaultBaseline": "commit",
  "executionModes": ["local", "worktree"],
  "baselines": ["commit", "current_working_tree"] }
```

For an isolated `session/new`, the response's
`_meta.querymt.sessionWorkspace` discloses the resolved binding: `sessionCwd`
(the directory the session actually executes in), `workspaceId`, `branch`,
`baseline`, `baselineCommit`, `localChangesExcluded` and
`ignoredUntrackedFilesExcluded`. Local sessions carry no such metadata.

Clients that cannot send creation metadata use the documented flow: submit
`/worktree create` from any session, then load the returned session id; the
loaded session runs in its bound checkout. No live switching is involved.

## Native commands and typed operations

Submit these as ordinary ACP `session/prompt` text, not queued or steering
input. They are intercepted before prompt expansion and never ask the model
to run Git commands:

```text
/worktree create
/worktree create HEAD review
/worktree create --include-local-changes review
/worktree handoff [name] [--summary]
/worktree list
/worktree status <workspace-id>
/worktree result <workspace-id>
/worktree publish [workspace-id]
/worktree archive <workspace-id>
/worktree remove <workspace-id>
/worktree recover <workspace-id>
```

`create` returns a **new session ID**, workspace ID, checkout path, branch,
and baseline. Attach to that new session to work there: the conversation
issuing the command does not switch roots. The optional name labels the new
session, not its managed branch. Names and refs are single whitespace-separated
tokens. With `--include-local-changes`, an optional name is accepted but a base
ref is not. Without that flag local edits are excluded. Ignored untracked
files are always excluded. `status`, `result`, and `publish` default to the
current session's workspace when no ID is supplied; local sessions must supply
an ID. Results are also returned in prompt response `_meta["querymt.worktree"]`.

The command's output is also delivered as an `agent_message_chunk` session
update on the issuing session (an ephemeral event, never journaled or replayed)
before the prompt response, on stdio and WebSocket alike, so every client
attached to the session sees it. No client bridge route is involved. The legacy
`AcpServer::run_stdio` forwarder does not track per-connection delivery, so
there the update is sent but its ordering against the response is not
guaranteed.

The same service backs additive ACP extension methods:

| Method | Parameters |
|--------|------------|
| `querymt/worktree/create` | `{"cwd":"/path/to/repo", "include_local_changes":true, "name":"review"}` |
| `querymt/worktree/handoff` | `{"session_id":"<owner>"}` or `{"workspace_id":"<id>"}`, optional `"name"`, `"summary":true` |
| `querymt/worktree/list` | `{}` |
| `querymt/worktree/status` | `{"workspace_id":"<id>"}` |
| `querymt/worktree/result` | `{"workspace_id":"<id>"}` |
| `querymt/worktree/publish` | `{"workspace_id":"<id>"}` or `{"session_id":"<id>"}` |
| `querymt/worktree/archive` | `{"workspace_id":"<id>"}` |
| `querymt/worktree/remove` | `{"workspace_id":"<id>"}` |
| `querymt/worktree/recover` | `{"workspace_id":"<id>"}` or `{"session_id":"<id>"}` (pending isolated forks) |

Commit-based creation also accepts `base_ref` (or `baseRef`). Result responses
carry the immutable baseline/result OIDs and retained result refs, the
baseline-relative changed paths with change kinds, the verification state, and
the **captured HEAD**; live HEAD and dirty status are reported separately. An
empty result list does not imply integration. Session views include workspace
provenance and distinct execution cwd values. Managed sessions group under
their workspace's **source cwd** (the project they were isolated from, which
survives archive and removal); local sessions group under their own cwd. A
durable `session_workspace_bound` event exposes the same binding without
changing existing ACP session updates.

`publish` is the explicit top-level publication entry point. It captures the
workspace's current uncommitted working state as an immutable result through
the same private-index capture used for delegates, so the real index, working
files, and every other checkout remain unchanged. Publication performs no
verification, which is reported as `skipped`. A failed capture reports an
explicit error and keeps the checkout untouched; it never records a partial
result. Inspection (`result`) is read-only and never republishes live state.

### Management from code

Every operation above is implemented once, as typed requests and responses
plus a typed error enum (`querymt_agent::worktree::WorktreeOps`). The ACP
extensions, the native `/worktree` command, the simple API and the Python
bindings are thin adapters over it, so ownership, lease and removal rules
cannot drift between surfaces. Existing ACP JSON shapes are unchanged.

=== "Rust"

    ```rust
    use querymt_agent::api::SessionOptions;

    // Explicit isolation; default options are a plain local session.
    let session = agent
        .chat_session_with(SessionOptions::worktree().name("refactor"))
        .await?;
    println!("{}", session.workspace().unwrap()["sessionCwd"]);

    // None when the agent has no workspace service.
    let worktrees = agent.worktrees().expect("enabled");
    let listed = worktrees.list().await?;
    ```

=== "Python"

    ```python
    session = await agent.chat_session(workspace="worktree", name="refactor",
                                       include_local_changes=True)
    print(session.workspace["sessionCwd"])      # None for a local session

    wt = agent.worktrees                         # None when not enabled
    await wt.list(); await wt.status(wid); await wt.result(wid)
    await wt.publish(workspace_id=wid); await wt.archive(wid); await wt.remove(wid)
    ```

    Failures raise `querymt_agent.WorktreeError` (a `RuntimeError`) carrying a
    `.code`.

#### Error codes

Every surface reports the same stable, machine-readable `code` (ACP error
`data.code`, `WorktreeOpError::code()`, Python `WorktreeError.code`). Human
readable messages may be reworded; codes do not change.

| Code | Meaning |
|------|---------|
| `workspace_service_unavailable` | isolation or an operation was requested but no service is configured (never downgraded to shared execution) |
| `invalid_options` | malformed or contradictory request (for example a ref with `current_working_tree`) |
| `isolation_unsupported_layout` | not a supported repository (non-Git, bare, sparse, submodule, ...) |
| `workspace_not_found` | unknown workspace id |
| `workspace_missing` | the bound checkout is gone; there is no fallback to the source |
| `conflicting_cwd` | load/resume with a cwd that contradicts the persisted binding |
| `owner_conflict` | another session already owns the writable workspace |
| `owner_active` | handoff refused: the owner has a running turn, queued input, shared delegates, or execution in another process |
| `owner_transferred` | the session handed its workspace off and is read-only; the current owner is in `ownerSessionId` |
| `not_owner`, `workspace_not_ready`, `owner_not_local` | handoff refused: the target does not own a ready, locally hosted workspace (nothing is created) |
| `summary_failed` | handoff with `summary` failed before the transfer; ownership is unchanged |
| `handoff_incomplete` | ownership moved but the successor is not runnable yet; run `recover` with its `session_id` |
| `removal_refused` | removal refused (active owner, dirty/untracked/ignored content, unpreserved commits) |
| `operation_refused` | another lifecycle operation was refused in the current state |
| `repository_error`, `storage_error`, `session_start_failed`, `internal_error` | infrastructure failures |

### Opt-in workflow

1. Configure the durable workspace service and create a session with
   `/worktree create --include-local-changes` if existing edits must be included.
2. Attach to its returned session ID. Its delegates inherit isolation; a local
   parent can explicitly request `"isolation":"isolated"` on the delegate tool.
3. Inspect the retained child ID/path and `/worktree result <id>`. Review only
   baseline-to-result changes using the exact OIDs/refs in the response. A
   top-level workspace publishes its own delta with `/worktree publish`.
4. Integrate that delta after review. Completion, cancellation, disconnection
   and commands never merge it for you; for delegates the parent session accepts
   it explicitly (see [Accepting a delegate result](#accepting-a-delegate-result)).
   A top-level workspace's delta is integrated manually.
5. Close the owning session and any explicitly shared delegates. From another
   local conversation, run `/worktree archive <id>`, then `/worktree remove <id>`.
   Archive permanently retires execution while preserving conversation history
   and its durable binding. Resume/load of an archived or removed workspace
   fails explicitly; archive is not a temporary pause. Checkout activity leases
   prevent archive/removal while any cooperating actor still runs, including
   actors in another process. Closing alone preserves normal resumability.
   Dirty, untracked, ignored, externally owned, or unpreserved-result checkouts
   remain protected. There is no `--force`; successful removal reports retained refs.

Removal safety does not depend on Git status configuration: untracked and
ignored files are detected even when `status.showUntrackedFiles=no` is set,
and any such content refuses removal with nothing deleted.

### Removal, external writers, and retained refs

The repository-administration lock coordinates QueryMT processes only; plain
Git commands and editors never honor it. Removal therefore assumes external
writers and is race-safe by construction:

- The managed branch is observed during the preservation checks and deleted
  with an **expected-target transaction** (delete only while it still points
  at the observed commit). If an external writer moves the branch between
  check and deletion, the branch — together with its new history — is
  **retained, never deleted**, and the removal response discloses the race in
  `branch_moved_during_removal` plus the branch in `retained_refs`.
- Removal responses always report exactly what remains:
  `checkout_removed`, `removed_branch`, `branch_moved_during_removal`, and
  `retained_refs`. A response never claims complete deletion when anything
  was retained or refused; baseline refs of removed workspaces are always
  retained and listed.
- Interrupted `removing` state recovers through the same safety checks, run
  freshly: a branch moved while removal was interrupted is refused as
  unpreserved until its commits are contained in a retained result ref, and
  an already-deleted branch or vanished checkout is completed idempotently
  without unconditional deletion.

For an interrupted `provisioning` or `removing` operation, use
`/worktree recover <id>` (or its typed extension). Recovery completes the
persisted intent conservatively and never starts an actor or changes its owner.
Pending isolated forks recover by session id through the typed extension:
`querymt/worktree/recover` with `{"session_id":"<id>"}`.

### Provisioning phases and recovery

Provisioning records its progress durably after every step, so a crash at any
boundary can be replayed from the recorded state instead of guessing:

1. **Intent persisted** — the workspace row exists with its source identity
   (source checkout and repository), requested baseline policy and ref,
   relative cwd, intended branch and checkout path, and a unique operation id.
   Nothing has been captured or allocated yet.
2. **Baseline bound** — the immutable baseline identity is durably bound.
3. **Baseline ref published** — `refs/qmt/baselines/<workspace-id>` exists at
   the bound baseline.
4. **Branch created** — the managed branch exists at the baseline commit.
5. **Checkout created** — the linked worktree exists; only the final
   `ready` transition remains.

Recovery behavior depends on how far the interrupted allocation got:

- **Intent-only (no baseline bound).** The recorded source and policy are
  sufficient to finish: recovery re-resolves the recorded ref (or source
  HEAD) or re-runs the validated current-working-tree capture against the
  recorded source checkout. The source must still exist and be a usable
  repository — recovery never fabricates a baseline. If the source is
  unavailable or the recorded ref no longer resolves, the workspace stays
  non-runnable (`provisioning`) with an actionable diagnostic in
  `last_error`, and a later explicit retry succeeds once the source is back.
- **Recovery-time captures are disclosed.** A baseline captured during
  recovery reflects the source contents at recovery time — not the state
  before the interruption — and is reported as
  `baselineSource: "captured-during-recovery"`; a baseline that was already
  bound before the crash is reported as `"bound-before-interruption"` and is
  preserved exactly. Recovery never recaptures or replaces a bound baseline.
- **Retained baseline refs are repaired, never rewritten.** If the process
  crashed after binding the baseline but before publishing its ref, recovery
  recreates `refs/qmt/baselines/<workspace-id>` at that exact baseline. If a
  ref with that name already exists at a different commit, recovery refuses
  with an explicit conflict instead of overwriting it; resolve the conflict
  manually, then retry.
- **No duplicates, no silent startup.** Recovery reuses the recorded branch,
  checkout path, and operation id — including branches or checkouts that
  were already created before the crash — and never registers a second
  workspace row or a second writable owner for the same checkout.
  Pre-existing directories at the checkout path that QueryMT does not own
  are preserved untouched. Recovery never launches an actor; only a fully
  provisioned and durably bound workspace becomes runnable, and a pending
  isolated fork still cannot load or resume as local execution.

## Resuming and loading

A session's workspace binding is persisted independently of conversation
contents. Resume/load always resolve that binding first:

- The session runs in its **persisted checkout** — the same path across
  restarts — regardless of where the client reattaches from. The binding,
  not any caller-supplied path, is the single authority for the effective
  working directory; tool context, indexes, hooks, and delegation all
  resolve it the same way.
- A **conflicting `cwd` request fails** with a workspace-specific diagnostic;
  it is never silently re-bound to a different directory.
- A **missing checkout fails closed**. Deleted worktrees are recorded
  (lifecycle `missing`) and the session refuses to attach, rather than
  falling back to the original checkout.

One workspace has exactly **one writable owner session**. A second session
cannot attach to an owned checkout; it creates its own through an explicit
creation request.

## Workspace-scoped slash commands

Slash commands follow the effective workspace. A managed session discovers
its commands from its **bound checkout**, not from the directory the agent
process was started in:

- **Project commands** (`.qmt/commands/*.md`) are re-resolved from the
  checkout for both the advertised catalog and the actual `/command`
  expansion, so the two always agree. Two checkouts of the same project can
  define the same command differently, and each session expands its own
  version.
- A command that exists **only in an isolated checkout** can be invoked in
  that session; the original checkout does not consult the isolated one.
- **Global and configured sources** (for example `~/.qmt/commands` and
  explicitly configured paths) follow the same discovery configuration as
  agent startup, and the scripts policy (enabled/disabled) is unchanged.

```console
# In the original checkout, /deploy may read:
#   "Deploy to staging"  (project command committed on main)
/worktree create feature-x
# In the isolated session, the same .qmt/commands/deploy.md now reads:
#   "Deploy the service"  (as defined in that checkout)
/deploy api
```

Absolute paths inside command prompts are **not rewritten** — a prompt that
hard-codes `/Users/me/project/src` keeps referring there, so prefer
repository-relative paths in shared command files. The native `/worktree`
command family keeps its built-in interception and is never expanded as a
prompt command, and legacy local sessions continue to use the registry
built at agent startup.

## Forking

Forking a session copies its conversation up to a message fork point, as
before. An isolated source automatically gives the fork its own fresh
workspace, even without metadata. A local source can request isolation explicitly:

- The fork's checkout reflects the source's **current working contents** by
  default — not the historical filesystem state at the fork point. The
  response discloses this via
  `_meta.querymt.sessionWorkspace.disclosure = "fresh-working-state"` and
  includes the fork's `workspaceId` and `sessionCwd`.
- The fork's persisted session cwd **is** its bound checkout: the row, the
  workspace binding, listing, load, and resume all agree with no external
  rewrite. Historical absolute paths in copied messages are context only —
  they are not rewritten, and there is **no live workspace switching**.
- The message fork point and the workspace association are recorded
  **separately**: selecting an older message does not rewind files.
- Forks never share a checkout with the source, a sibling, or the parent.
- Forks of local sources without workspace options remain pure message copies.
- An explicit local fork of an isolated source is rejected rather than sharing its checkout.

### Pending and failed fork creation

Creation is fail-closed at every boundary. An invalid request creates no
fork row and copies no history. Once the conversation copy commits, a
durable **startup barrier** (a pending isolation intent) travels with the
fork until its workspace is allocated, bound as the single writable owner,
and the execution cwd is published atomically in SQLite:

- A pending or failed fork is **not runnable**: load and resume reject it
  with `fork isolation is pending; refusing local fallback` instead of
  running it in the source checkout — including after a restart or when no
  workspace service is configured.
- Failures compensate only newly allocated, unused resources: a workspace
  whose owner binding failed is marked `failed` (diagnostic retained, Git
  resources preserved for explicit recovery/removal); the copied history is
  always preserved.
- Session **listing flags these forks** (`pending_isolation`), and the
  workspace view carries the failed lifecycle when one exists, so a pending
  fork is never mistaken for a legacy local session.
- **Recovery is explicit** and creates no duplicate owners:
  `querymt/worktree/recover` with `{"session_id": ...}` completes a pending
  fork — reusing its bound workspace when a previous attempt already
  attached it, otherwise allocating one (with a baseline captured at
  recovery time, disclosed in the response) — persists the execution cwd,
  and releases the barrier in one validated SQLite transaction. Workspace
  records with interrupted provisioning/removal intents recover by id via
  `/worktree recover <workspace-id>` as before. A recovered fork runs in its
  own checkout; retries are refused once nothing is pending.

## Ownership handoff

A managed checkout has one writer, but a long conversation may want a fresh
context without moving any files. **Handoff** starts a new session in the
*same* checkout and moves ownership to it. No running session changes root:
ownership moves to a new session. Files, branch, index and baseline are
untouched.

```text
/worktree handoff            # clean successor, no history
/worktree handoff next       # name the successor
/worktree handoff --summary  # seed it with a summary of this conversation
```

=== "ACP"

    ```json
    {"method": "querymt/worktree/handoff",
     "params": {"session_id": "<owner>", "name": "next", "summary": true}}
    ```

=== "Rust"

    ```rust
    use querymt_agent::worktree::{HandoffOptions, WorkspaceTarget};

    let handed = agent.worktrees().expect("enabled")
        .handoff(WorkspaceTarget::Session(owner), HandoffOptions::default())
        .await?;
    println!("{} now owns {}", handed.session_id, handed.checkout_path);
    ```

=== "Python"

    ```python
    handed = await agent.worktrees.handoff(session_id=owner, summary=True)
    print(handed["session_id"], handed["summary_seeded"])
    ```

The response carries `session_id`, `previous_session_id`, `workspace_id`,
`checkout_path`, `session_cwd`, `branch` and `summary_seeded`. The successor is
listed nested under the previous owner (fork origin `worktree`).

**Preconditions.** The workspace is `ready`, owned, and the owner is hosted
locally. A loaded, idle owner is unloaded so it releases its shared activity
lease, then the exclusive lease must be obtainable; a running turn, queued or
steered input, shared delegates, or another process using the checkout refuse
the handoff with `owner_active` and create nothing.

**Atomicity.** The successor is persisted behind the same durable startup
barrier as isolated forks, ownership moves with one conditional update
(`owner = previous AND lifecycle = ready`, so racing handoffs have exactly one
winner), and only then is the barrier released. A crash before the transfer
leaves the previous owner in place; a crash after it leaves a successor that
owns the workspace but stays non-runnable. In both cases
`querymt/worktree/recover` with the successor's `session_id` completes the
transfer (or confirms it) and releases the barrier; retries create no second
owner and the successor never runs unowned.

**Summary.** Clean by default. With `summary`, the compaction summarizer runs
over the previous conversation *before anything is created* and the result
seeds the successor as its initial context (`summary_seeded: true`). A
summarizer failure returns `summary_failed` with ownership unchanged.

### Read-only previous owners

After a handoff the previous owner stays listable and loadable, but it never
materializes again, so it cannot reload as a local or shared session inside
the checkout it gave away:

- `session/load` replays its history and discloses
  `_meta.querymt.sessionAccess = {"mode": "read_only", "reason":
  "owner_transferred", "ownerSessionId": "<current owner>"}`.
- Prompts (including native commands), queued or steered input, undo/redo and
  forks are refused with `owner_transferred` before any tool can run; worktree
  operations addressed by that session id report the same code.
- Its snapshots are context-only: undo and redo are refused, so they can never
  touch the successor's files.
- Loading it needs no actor: history and the event journal are read directly,
  and no configuration update is sent because there is no live configuration.

**Handoff announcement.** Committing a handoff records a durable
`worktree_ownership_transferred` event on the previous owner's stream and sends
it as an `agent_message_chunk` with the stable `message_id`
`worktree-handoff-<workspace_id>-<successor>` and
`_meta.querymt.worktreeHandoff = {workspaceId, successorSessionId,
summarySeeded}`. Every client attached to the previous owner receives it before
a native `/worktree handoff` response, and it is replayed after the history
whenever the session is loaded. If the event cannot be recorded the failure is
logged and the handoff still succeeds.

**Compatibility.** The new durable event kind makes the journal of a session
that has been handed off unreadable by binaries that predate it (the same
precedent as `session_workspace_bound`); upgrade every node sharing a database
before using handoff. Profile runtimes built from one `AgentInfra` share its
event fanout and storage journal, so announcements reach connections served by
the root agent; runtimes given separate fanouts are only forwarded if they are
registered as event sources.

## Undo, snapshots, and workspace references

Undo and redo reuse the **existing snapshot backend**, unchanged. The
backend is bound to the session's effective cwd, so each workspace keeps
its own undo history, and undo/redo never moves project branch refs or
restores files in sibling or original checkouts.

Snapshot coverage stays exactly as before isolation: snapshots capture
the working files inside the session's cwd scope (hidden files and
gitignored paths are not captured), and undo history is not a complete
export of Git working state.

Two kinds of Git references exist side by side and do not interfere:

- **Snapshot commits** live in a shadow repository under QueryMT's data
  directory and power undo/redo only. They are not project branches and
  are not used as baselines or results.
- **Workspace refs** (`refs/qmt/baselines/<workspace-id>`, plus retained
  result references) are real refs of your project repository. They keep
  workspace baselines and delegate results reachable for review and
  recovery across restarts.

Snapshot garbage collection prunes only the shadow snapshot history. It
never removes workspace baseline or result references, so collected undo
history does not affect result review or recovery.

Snapshots can also be **disabled entirely**: workspace creation, isolated
delegation, and the worktree command surface keep working without a
snapshot backend, and undo reports its usual unavailable-backend behavior
rather than substituting workspace Git refs as undo history. Enabling
isolation never silently enables snapshots when they were disabled.

## Accepting a delegate result

A session that has reviewed an isolated delegate's retained result accepts it
with the `accept_delegation_result` tool (`delegation_id` and the reviewed
`result_commit`; no paths, commands or messages). Integration is deterministic
runtime code, not a prompt:

- The delegate's change is exactly `baseline → result`. It is applied with a
  three-way merge against the checkout's **current** `HEAD`, so when parallel
  siblings share one baseline, the one accepted second is layered on top of the
  one accepted first. Overlapping edits report a **conflict** and change nothing.
- The change becomes **one commit on the checkout's current branch** (the branch
  is advanced directly). The message names the objective and carries
  `QueryMT-Delegation`, `QueryMT-Workspace`, `QueryMT-Result` and
  `QueryMT-Baseline` trailers. The author and committer come from the
  repository's Git configuration (system, global and user config are read; a
  missing identity is refused, none is invented).
- Local work is never swept in: only the paths in the delta are written and
  committed. If any of them has staged, unstaged, untracked or ignored local
  state, or the delegate's baseline already contained uncommitted edits to it,
  the request is **refused** and nothing changes. Detached `HEAD`, an
  in-progress merge/rebase/cherry-pick, sparse checkout, submodule changes and
  unresolved conflicts are also refused.
- Accepting the same result again returns the original commit
  (`refs/qmt/integrations/<workspace-id>/<result-commit>`); only the delegation's
  parent session may accept, and never into the delegate's own workspace.
- A refusal or conflict is final for that result. Delegate the underlying change
  again from the current state rather than patching by hand.

### How an acceptance runs

No Git executable is involved. The whole transaction runs under the requesting
session's checkout mutation gate (a checkout-keyed gate under the storage root
when the session has none) and holds the checkout's real `index.lock` from the
local-work check until the index is updated:

1. Recover an unfinished earlier acceptance on the same checkout (see
   [Recovery](#recovery-of-an-interrupted-acceptance)), then check the replay
   record.
2. Preflight, compute the delta `baseline → result`, three-way merge it with
   `HEAD`, and check local work.
3. Persist an intent file in the checkout's per-worktree Git directory
   (`qmt-integration-intent.json`) and write exactly the touched files, with
   attribute filters, executable bits and symlinks.
4. Run the hooks, then re-read what they staged, commit, and move the branch and
   create the integration record in **one compare-and-swap reference
   transaction**.
5. Update the real index for the touched paths, release the lock, remove the
   intent, then run `post-commit`.

### Hooks

`pre-commit`, `prepare-commit-msg <file> message` and `commit-msg <file>` run from
`core.hooksPath` (default `<common dir>/hooks`) after the files are on disk, so
they see the result. They run with the checkout root as working directory, a
private index holding the merged tree as `GIT_INDEX_FILE`, `GIT_EDITOR=:`, and
every other inherited `GIT_*` variable removed. A hook that is not an executable
file is skipped, as Git does. Each hook is limited to 600 seconds and its whole
process tree is killed on timeout. On Windows hooks run through `sh`; a present
hook without `sh` is refused (`hook_requires_sh`).

Differences from `git commit`:

- There is **no `--no-verify`**: hooks cannot be skipped, by the model or anyone
  else.
- `post-commit` runs after the commit is final; its failure is logged and the
  acceptance still succeeds.
- A hook may re-stage files this acceptance **writes** (a formatter, for
  example) and the staged content is committed. Staging anything else is
  refused (`hook_changed_paths_outside_delta`), including delta paths an earlier
  sibling already integrated, because those were never checked for your local
  work. That staging is left in the private index only, and nothing is
  committed.
- A hook that edits a written file **without** re-staging it commits the
  applied content; the edit stays in your working tree and shows up in
  `git status` (the index entry is not given the edited file's stat data).
- Hooks see the whole working tree, including your unrelated uncommitted
  work, which looks "unstaged" relative to the private index. Hook managers
  that stash or restore unstaged changes around the hook (pre-commit,
  lint-staged) will touch those files; if such a hook is killed by the
  timeout mid-stash, recover your work from `git stash list`. Commit or stash
  unrelated work first when such hooks are installed.
- The commit message is cleaned like `git commit -m` (trailing whitespace
  stripped, blank runs collapsed); an empty message is refused
  (`empty_commit_message`).
- Hook managers that rely on variables Git sets for `git commit` itself may
  behave differently.
- **Commit signing is not supported.** With `commit.gpgSign=true` the request is
  refused (`signing_unsupported`) before anything is written; QueryMT never
  creates an unsigned commit instead.

### Refusal codes

Every refusal leaves the branch, the index contents and your files as they were
(the one exception, `hook_changed_paths_outside_delta`, leaves the hook's own
edits in place).

| Code | Meaning |
| --- | --- |
| `invalid_result_commit`, `ambiguous_result_commit`, `result_not_found`, `delegation_not_complete`, `no_isolated_workspace`, `no_baseline`, `not_parent_session`, `wrong_repository`, `wrong_target` | The request does not match durable state |
| `integration_record_unreachable` | A record exists for this result, but its commit is not on the current branch (for example after a reset); delete the ref named in the message to accept again |
| `unborn_head`, `detached_head`, `operation_in_progress`, `sparse_checkout`, `unknown_object`, `missing_identity`, `signing_unsupported`, `unsupported_gitlink`, `unsupported_filter` | The checkout or result cannot be integrated natively |
| `index_locked` | An `index.lock` exists that QueryMT did not create |
| `unmerged_entries`, `touches_local_changes`, `baseline_local_edits` | Local work is in the way |
| `integration_pending` | An intent file appeared between recovery and apply (another writer outside the gate) |
| `branch_moved`, `head_changed` | The branch moved (or `HEAD` left it) while the acceptance ran, or an earlier interrupted acceptance cannot be resolved because of it |
| `touched_paths_edited` | An interrupted acceptance would be rolled back, but files it wrote were changed since |
| `commit_rejected` | A hook exited non-zero, timed out or could not start; the message names the hook and its output |
| `hook_changed_paths_outside_delta`, `empty_commit_message`, `hook_requires_sh` | The hook phase did not produce a committable result |

A conflict is reported separately and lists the overlapping paths.

### Recovery of an interrupted acceptance

If the process dies mid-acceptance, the intent file names what was in flight and
the next acceptance on the same checkout resolves it first, under the gate:

- **The branch is still at the recorded `HEAD`** (the usual case): the touched
  paths are restored to `HEAD`, a record left by a torn reference transaction is
  deleted if it still names the abandoned commit, and the scratch files and
  intent are removed. Your other files are never touched. If any touched file
  now holds content that is neither what the acceptance wrote nor what `HEAD`
  has (you kept working after the crash), nothing is rolled back: recovery
  refuses with `touched_paths_edited`, naming the files. Move or restore them,
  then retry.
- **The branch is at the recorded commit** (the process died after the reference
  transaction): the record is recreated if missing and the real index is brought
  in line with the commit. The acceptance then reports `Integrated` with that
  commit. `post-commit` does not run in this case.
- **Anything else** (the branch moved to some other commit): recovery refuses
  with `branch_moved` and keeps the intent and your files for inspection.

A reference transaction that fails part-way (one of branch and record updated,
the other not) is not rolled back on the spot; the intent is kept and the same
recovery finishes or undoes it consistently.

To give up on an intent that cannot be recovered (after inspecting it), delete
`qmt-integration-intent.json` and any `qmt-integration-*` scratch files from the
checkout's Git directory (`git rev-parse --git-dir`), and restore the touched
paths it lists yourself.

A surviving `index.lock` is removed only when its identity (device, inode and
creation time) matches the one recorded in the intent, which proves the
interrupted acceptance created it. Any other lock, including a stale one from a
Git process, is never removed; the acceptance is refused with `index_locked`
until you remove it yourself. A hook rejection or a failure before the reference
transaction is rolled back in the same call while the acceptance still holds the
index lock, so a failed acceptance does not leave applied files behind; if that
rollback cannot run, the error says so and the next acceptance retries it.

Delegation completion does not mean the objective succeeded: a delegate that
stopped without changing anything reports a result with no baseline-relative
changes, and the summary says so instead of offering it for acceptance.

## Reviewing and integrating results

Nothing is ever merged back automatically. Delegates publish their uncommitted
output automatically on completion; top-level workspaces publish explicitly
with `/worktree publish` or `querymt/worktree/publish`. Either way the
workspace — checkout path, managed branch, baseline, and a captured immutable
result — is retained exactly as it was, and the completion, failure,
cancellation, and summary output identify what was retained.

A retained result is **baseline-relative**. The workspace started from a
private baseline commit (which included the parent's working state at dispatch
for delegates); the captured result contains that baseline plus the
workspace's own work. Review reports the baseline-to-result changes without
consulting the live checkout:

- `baseline_commit` / `result_commit` and the retained `result_ref`
  (`refs/qmt/results/<workspace-id>/<result-id>`),
- `changed_paths` with change kinds (`added`, `modified`, `deleted`),
- `captured_head` — the checkout HEAD when the result was captured — and the
  `verification` state (`passed`, `failed`, `pending`, or `skipped`),
- live `live_head`/`dirty` state separately; later edits or commits never
  rewrite an existing result, and each new publication gets its own
  create-only ref.

You can also reproduce the delta with Git directly:

```sh
git diff --name-status "refs/qmt/baselines/<workspace-id>" \
                        "refs/qmt/results/<workspace-id>/<result-id>"
```

Example: a parent with uncommitted edits to `config.rs` dispatches an
isolated delegate that changes `auth.rs` and leaves `notes.md` untracked.
The baseline already contains the `config.rs` edit, so the baseline→result
review lists exactly `auth.rs` and `notes.md` — the delegate's delta — and
never reports the inherited `config.rs` change as delegate work. The same
applies to an explicit top-level publication, which reports only that
workspace's changes since its baseline.

!!! warning "Do not raw-merge a private branch from a dirty parent"
    When the baseline captured uncommitted parent edits, result responses set
    `baseline_includes_uncommitted_edits` and include an
    `integration_warning`. The managed branch points at (or beyond) that
    baseline, so `git merge <managed-branch>` (or cherry-picking its tip
    wholesale) can bring the parent's baseline edits along with the
    workspace's work. Integrate the delta deliberately instead:

    ```sh
    # Apply only the published delta onto your current branch:
    git diff "refs/qmt/baselines/<workspace-id>" \
             "refs/qmt/results/<workspace-id>/<result-id>" | git apply
    ```

    or, if the workspace produced real commits and the parent was clean,
    cherry-pick the commit range. If the parent was dirty, either commit the
    parent's state coherently first, or apply the baseline-relative patch as
    above. A clean worktree or a completed status never implies the result has
    been applied or accepted — review the delta and integrate it yourself.

## What isolation does not do

- **No sandbox confinement.** See the warning above — tools are not
  restricted to the workspace by the isolation feature.
- **No live switching.** An existing session's workspace is never switched
  in place. Creating a worktree through the command surface, forking, or
  delegating always yields a *new* session identity bound to a new checkout;
  the current conversation keeps its existing workspace.
- **No automatic integration of results.** Completed or failed delegates
  keep their edits in their own checkout for review; nothing is merged back
  automatically.

## Editor and external integrations

- **MCP roots must be verified before startup.** Implement
  `SessionMcpAttachmentSource::validate_workspace_roots` to validate all
  configured/request/attached servers and connected peers against the supplied
  effective cwd. The default rejects nonempty MCP configurations for managed
  sessions because arbitrary absolute arguments, HTTP servers, and connected
  peers may retain another root. Validated stdio servers launch in the checkout.
  Local sessions retain their existing MCP behavior.
- **Editor workspace-query bridges** (e.g. a language-query integration
  bound to the folder your editor has open) are disabled for workspace-bound
  sessions with an explicit diagnostic, instead of silently answering
  language queries from the original checkout.
- **Remote filesystems** are unsupported for isolated execution in this
  version: placement is validated before any Git administration runs, and
  requested isolation on a remote filesystem path fails before dispatch.
  Remote *model inference* with local filesystem execution is unaffected.

## See also

- [Slash Commands](slash-commands.md) — command discovery and format
- [Delegation](delegation.md) — multi-agent delegation
- [API Reference](api_reference.md) — session creation and fork requests
