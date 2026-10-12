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

Workspace isolation is opt-in. Configure a `WorkspaceService` on the agent
builder; without it every session executes locally exactly as before:

```rust
use std::sync::Arc;
use querymt_agent::worktree::{CliGit, WorkspaceService};

let storage_root = dirs::data_dir().unwrap().join("qmt/worktrees");
let service = Arc::new(WorkspaceService::new(
    Arc::new(CliGit::new()),
    storage.workspace_repository().expect("backend exposes a workspace repository"),
    storage_root, // durable worktree storage, e.g. ~/.local/share/qmt/worktrees
));

let config = AgentConfigBuilder::from_provider(storage, provider, journal)
    .with_workspace_service(service)
    .build();
```

Managed checkouts live under `<storage-root>/<repo-slug>/<workspace-id>/`,
survive session completion, process restarts, and machine reboots.

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
  `~/.local/share/qmt/worktrees/project/ws_01ABCdef…` — and that path, not
  the source directory, is what file tools, the shell, indexes, prompts, and
  hooks use. Ignored (gitignored) untracked files are never copied; the
  response discloses that exclusion for current-working-tree baselines.
- **The source checkout is untouched.** HEAD, branch, index, stash, and
  working files of the original directory are never modified by creation.
- **Requested isolation never falls back.** If Git, the repository, durable
  storage, or local filesystem placement is unavailable, creation fails with
  an actionable error instead of silently running shared.

## Native commands and typed operations

Submit these as ordinary ACP `session/prompt` text, not queued or steering
input. They are intercepted before prompt expansion and never ask the model
to run Git commands:

```text
/worktree create
/worktree create HEAD review
/worktree create --include-local-changes review
/worktree list
/worktree status <workspace-id>
/worktree result <workspace-id>
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
files are always excluded. `status` and `result` default to the current
session's workspace when no ID is supplied; local sessions must supply an ID.
Results are also returned in prompt response `_meta["querymt.worktree"]`.

The same service backs additive ACP extension methods:

| Method | Parameters |
|--------|------------|
| `querymt/worktree/create` | `{"cwd":"/path/to/repo", "include_local_changes":true, "name":"review"}` |
| `querymt/worktree/list` | `{}` |
| `querymt/worktree/status` | `{"workspace_id":"<id>"}` |
| `querymt/worktree/result` | `{"workspace_id":"<id>"}` |
| `querymt/worktree/archive` | `{"workspace_id":"<id>"}` |
| `querymt/worktree/remove` | `{"workspace_id":"<id>"}` |
| `querymt/worktree/recover` | `{"workspace_id":"<id>"}` |

Commit-based creation also accepts `base_ref` (or `baseRef`). Result responses
contain immutable baseline/result OIDs, retained result refs, and verification
status; an empty result list does not imply integration. Session views include
workspace provenance and distinct execution cwd values. Managed sessions group
under `repository:<canonical-common-dir>`; local sessions retain cwd grouping.
A durable `session_workspace_bound` event exposes the same binding without
changing existing ACP session updates.

### Opt-in workflow

1. Configure the durable workspace service and create a session with
   `/worktree create --include-local-changes` if existing edits must be included.
2. Attach to its returned session ID. Its delegates inherit isolation; a local
   parent can explicitly request `"isolation":"isolated"` on the delegate tool.
3. Inspect the retained child ID/path and `/worktree result <id>`. Review only
   baseline-to-result changes using the exact OIDs/refs in the response.
4. Integrate that delta manually after review. No completion, cancellation,
   disconnection, or command merges it for you.
5. Close the owning session and any explicitly shared delegates. From another
   local conversation, run `/worktree archive <id>`, then `/worktree remove <id>`.
   Archive permanently retires execution while preserving conversation history
   and its durable binding. Resume/load of an archived or removed workspace
   fails explicitly; archive is not a temporary pause. Checkout activity leases
   prevent archive/removal while any cooperating actor still runs, including
   actors in another process. Closing alone preserves normal resumability.
   Dirty, untracked, ignored, externally owned, or unpreserved-result checkouts
   remain protected. There is no `--force`; successful removal reports retained refs.

For an interrupted `provisioning` or `removing` operation, use
`/worktree recover <id>` (or its typed extension). Recovery completes the
persisted intent conservatively and never starts an actor or changes its owner.

## Resuming and loading

A session's workspace binding is persisted independently of conversation
contents. Resume/load always resolve that binding first:

- The session runs in its **persisted checkout** — the same path across
  restarts — regardless of where the client reattaches from.
- A **conflicting `cwd` request fails** with a workspace-specific diagnostic;
  it is never silently re-bound to a different directory.
- A **missing checkout fails closed**. Deleted worktrees are recorded
  (lifecycle `missing`) and the session refuses to attach, rather than
  falling back to the original checkout.

One workspace has exactly **one writable owner session**. A second session
cannot attach to an owned checkout; it creates its own through an explicit
creation request.

## Forking

Forking a session copies its conversation up to a message fork point, as
before. An isolated source automatically gives the fork its own fresh
workspace, even without metadata. A local source can request isolation explicitly:

- The fork's checkout reflects the source's **current working contents** by
  default — not the historical filesystem state at the fork point. The
  response discloses this via
  `_meta.querymt.sessionWorkspace.disclosure = "fresh-working-state"` and
  includes the fork's `workspaceId` and `sessionCwd`.
- The message fork point and the workspace association are recorded
  **separately**: selecting an older message does not rewind files.
- Forks never share a checkout with the source, a sibling, or the parent.
- Forks of local sources without workspace options remain pure message copies.
- An explicit local fork of an isolated source is rejected rather than sharing its checkout.

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

## Reviewing and integrating delegate results

Nothing is ever merged back automatically. When an isolated delegate
finishes, its workspace — checkout path, managed branch, baseline, and (for
uncommitted output) a captured result — is retained exactly as it was, and
the completion, failure, cancellation, and summary output identify what was
retained.

A retained result is **baseline-relative**. The delegate started from a
private baseline commit (which included the parent's working state at
dispatch); the captured result contains that baseline plus the delegate's
own work. To see only what the delegate did, diff the two refs:

```sh
git diff --name-only "refs/qmt/baselines/<workspace-id>" \
                      "refs/qmt/results/<workspace-id>/<result-id>"
```

Example: a parent with uncommitted edits to `config.rs` dispatches an
isolated delegate that changes `auth.rs` and leaves `notes.md` untracked.
The baseline already contains the `config.rs` edit, so the baseline→result
diff lists exactly `auth.rs` and `notes.md` — the delegate's delta — and
never reports the inherited `config.rs` change as delegate work.

!!! warning "Do not raw-merge a private branch from a dirty parent"
    The delegate's managed branch points at (or beyond) its baseline, and
    the baseline embeds the parent's uncommitted edits at dispatch time.
    `git merge <managed-branch>` (or cherry-picking its tip wholesale) can
    therefore bring the parent's baseline edits along with the delegate's
    work. Integrate the delta deliberately instead:

    ```sh
    # Apply only the delegate's delta onto your current branch:
    git diff "refs/qmt/baselines/<workspace-id>" \
             "refs/qmt/results/<workspace-id>/<result-id>" | git apply
    ```

    or, if the delegate produced real commits and the parent was clean,
    cherry-pick the delegate's commit range. If the parent was dirty, either
    commit the parent's state coherently first, or apply the baseline-relative
    patch as above. A clean worktree or a completed status never implies the
    result has been applied or accepted — review the delta and integrate it
    yourself.

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

- [Delegation](delegation.md) — multi-agent delegation
- [API Reference](api_reference.md) — session creation and fork requests
