# Experimental OptChat backend memory

This is an independently implemented, session-scoped experiment based on the
OptChat/UniiChat summary-tree algorithm. It is not a port of pi-optchat's profile,
import, connected-window or subagent system.

## Enable

Add to a **single-agent** config:

```toml
[agent.memory]
enabled = true
strategy = "optchat"
scope = "session"
# Optional; otherwise <QMT_CONFIG_DIR|QMT_HOME|~/.qmt>/experimental-optchat
# Use an absolute path; relative paths use the server's working directory.
# directory = "/tmp/my-optchat-trial/memory"
```

The Rust API has `AgentBuilder::memory(MemorySettings { enabled: true, ..Default::default() })`.
There is no Cargo feature flag or frontend requirement. Builds without `dashboard`
can use `api` for the ACP WebSocket server. The experiment is enabled only by the
agent configuration; it is not a frontend/session toggle. Keep the same config
and memory directory when resuming a trial.

Use a **separate session database** while evaluating, e.g.:

```sh
qmtcode trial.toml --db /tmp/my-optchat-trial/sessions.db --acp-ws 127.0.0.1:3037
```

That command still uses your normal provider configuration/credentials. For fully
isolated tests also set HOME, QMT_CONFIG_DIR and XDG paths, and supply a provider
manifest in that config directory. Do not point test runs at a production DB.

## Behavior and boundaries

- The existing transcript remains authoritative and its schema is unchanged.
  OptChat uses an additional private SQLite sidecar named `<session-id>.sqlite3`
  with an exclusive OS lock, WAL and FULL durability. Foreign databases are rejected
  before schema/journal changes; schema and identity initialization are atomic.
  Newly created Unix directories/files are private, and symlink file aliases are
  rejected. No sidecar is opened, and no worker or memory tool is installed, while
  disabled.
- Normal user prompts, visible assistant messages, tool calls and results become
  durable memory entries. Reasoning, opaque provider state, snapshots and synthetic
  legacy compaction exchanges are excluded. Visible refusals and user/generated
  attachments are retained for zoom; adjacent prompt text blocks remain separated.
  Large ordinary text is split without dropping bytes; tool output is head/tail
  clipped to 30,000 characters. The normal tool-output cap/overflow policy still
  applies before OptChat receives a tool result; this is not a new raw-output archive.
- Nodes have a strict 512-byte default budget. Short input is preserved without
  inference; longer input and parent merges use bounded summary calls. Failed,
  incomplete, tool-calling, empty or still-oversized responses never become nodes.
  Ready jobs are persisted/indexed and concurrency is bounded across sessions.
  Completed calls are published immediately, even while a sibling waits; views are
  refreshed at batch boundaries. Cancellation retains the write gate until any
  already-started blocking SQLite write finishes. Provider error bodies are not
  saved in job diagnostics or included in OptChat background logs.
- A persisted ordered view covers the history, grows append-only between batches,
  and merges ready siblings when it exceeds 128,000 bytes down to 64,000 bytes.
  Due priority uses the pair's **inclusive last message**, normalized by child size;
  ties choose oldest. A smaller compactor view is persisted separately.
- Each new user turn waits for prior memory to catch up, then sees the frozen view
  plus only its own live messages/tool exchanges. Completion-hook continuations
  retain that view. Standard pruning/automatic compaction and ContextMiddleware
  are replaced only for the enabled agent; other middleware is retained.
- `zoom(id,n)` opens a node into its two child summaries; `zoom(id,1,offset)` pages
  an original (12,000 UTF-8 bytes/page; first page also includes attachments).
  `date(id)` returns its original timestamp. Both use the current session only.
  Tools are added to an explicit allowlist; a conflicting name/denylist or a
  provider-only tool policy is rejected rather than silently widening access.
- History removal/rewrite (including undo followed by a new prompt) starts a new
  sidecar generation. Old generations are retained on disk but cannot be retrieved
  by memory tools. Budget changes also create a generation. Forked sessions build
  their own memory from their own transcript. No cross-session/profile memory.
- Background ingestion/provider setup is scheduled at turn end without making
  completion wait for an earlier summary worker. Interrupted or cancelled turns are
  recovered from the normal durable transcript at the next user turn. A failed
  summary is retried on the next catch-up trigger, never in a tight retry loop.
  Catch-up failure/timeout fails the turn rather than silently losing history.
- Very long single turns stop at a conservative byte admission budget instead of
  invoking conventional compaction. Send a follow-up to continue with fresh memory.
  This is not exact token accounting and is not pi-optchat's automatic mid-turn
  refresh; use an adequate model context window and tune view sizes for small models.

## Advanced settings

```toml
[agent.memory]
enabled = true
view_min_bytes = 64000
view_max_bytes = 128000
summary_bytes = 512              # allowed: 512..4096
concurrency = 8                  # allowed: 1..8, shared across this agent's sessions
shortening_attempts = 5          # allowed: 1..5
request_timeout_secs = 60
catch_up_timeout_secs = 180
max_turn_bytes = 256000          # live turn allowance in addition to the view

# Optional independent summarizer; otherwise use the session's current provider/model.
# Authentication follows normal provider resolution; no separate secret is stored here.
[agent.memory.compactor]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
# [agent.memory.compactor.parameters]
# max_tokens = 512
```

Summarization makes additional potentially billable requests. First enabling it on
an existing long session can require substantial catch-up; start a new trial session.
Changing summary/view sizes creates a new generation and may repeat inference.

## Cache and experimental limitations

The main view uses stable four-line message blocks and a five-minute cache hint on
the last complete block, alongside the normal runtime's request-end hints. Exact
provider cache-hit ratios are **not guaranteed or measured**. Tool ordering, dynamic
system prompt content, hooks and provider serialization remain owned by QueryMT.
The summarizer uses the existing provider system instructions, a contextual summary
task and no callable tools; it does not implement the recipe's identical-tool prefix
or coalesce concurrent cache writes. Different models/accounts have separate caches.

The adapter currently reads existing full session history through SessionProvider
at turn boundaries for rewrite detection. The ready-job queue is incremental, but
end-to-end history loading is not yet an infinite-scale incremental cursor API.
Opened session handles are cached until this agent shuts down; this experimental
version has no idle eviction, so many thousands of sessions can exhaust resources.
Session initialization is per-session and does not hold the shared map lock over I/O.
Memory directories contain sensitive conversation data, including old generations,
and are not backups. Removing a session does not automatically delete its sidecar.
Mesh replication, shared profiles, UI status/browsing, imports, named subagent zoom,
per-summary cost UI and exact recall/cost benchmarking are out of scope.

## Database filenames

Each session uses its lowercase, hyphenated UUID directly:

```text
<memory-directory>/01a17a37-141b-7337-bd47-1fdc3647e193.sqlite3
<memory-directory>/01a17a37-141b-7337-bd47-1fdc3647e193.lock
```

SQLite can also create `<session-id>.sqlite3-wal` and
`<session-id>.sqlite3-shm`. Noncanonical UUIDs and path-like IDs are rejected
before accessing the memory directory. The database still checks its stored
session identity. Session display names do not affect these filenames.

Earlier experiment builds used `sha256(session-id).sqlite3`. There is no automatic
fallback or migration: existing hashed files are left untouched. To preserve an
existing trial's summaries, stop every backend using that memory directory,
back up the complete file set, and rename the hashed `.sqlite3` file to
`<session-id>.sqlite3`. Rename matching `.sqlite3-wal`, `.sqlite3-shm`, and `.lock`
files to the same session-ID prefix if present. Obtain the session ID from the
sidecar's `identity.session` field. Do not overwrite an existing destination or
rename an open database. Without this explicit rename, the new build creates
fresh memory and may repeat summarization. The main sessions database is unchanged.

## Disable or remove

Set `enabled = false` (or remove `[agent.memory]`) and restart the trial backend.
Normal history/context behavior resumes; original transcripts are not replaced by
OptChat summaries. Keep or delete the **separate trial memory directory** after
stopping that backend. No normal-database migration needs to be undone.

The removal surface is this directory plus its module declaration, the
`AgentSettings.memory` field, and `AgentBuilder` configuration/install wiring.
No execution-state, provider protocol, ACP or frontend types were changed.

## Validation

`cargo test -p querymt-agent --lib --features api,oauth,remote optchat`

Tests cover persisted view restoration, batch coverage, idempotence, failed-job
recovery, writer exclusion, undo generations, UTF-8 extraction/paging, rejected
summary outputs, disabled isolation and full two-turn agent/zoom execution. Review
regressions cover cancellation with unfinished siblings/blocking writes, nonblocking
background scheduling, foreign DB rejection, symlink lock bypass, invalid persisted
keys, visible refusal/media preservation and redacted configuration/error status.
Filename tests verify session-ID paths, persisted-memory reopen and rejection of
unsafe/noncanonical IDs before filesystem access.
Deterministic provider tests establish backend behavior, not real-model recall.
