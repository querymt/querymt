---
name: qmacp
description: "JSON CLI for a running QueryMT ACP WebSocket server (qmtcode --acp-ws). Find/create sessions, prompt, follow, steer, queue, configure, cancel, and inspect live protocol behavior. Use for operating live sessions, arranging agent callbacks from scripts/watchers, and testing/developing qmtcode."
license: MIT
compatibility: "Requires the qmacp binary on PATH and a running qmtcode --acp-ws. Uses the shell."
allowed-tools: shell
metadata:
  version: "0.2.2"
  tags: "querymt acp sessions automation orchestration callbacks qmtcode"
---

# qmacp

Each invocation connects, does one job, and exits. The server owns session state;
keep the returned `sessionId`. Default endpoint: `ws://127.0.0.1:3030/acp/ws`.

## Working contract

- Discover session, profile, model, mode, effort, and endpoint values with `caps`,
  `profiles`, `models`, `find`/`sessions`, or returned JSON; do not invent them.
  If multiple sessions match, inspect or ask rather than picking by list order.
- Do not close a session unless the user asked to end it or a qmtcode test requires
  `close`. Use ordinary tools to edit/test this qmacp repo, not qmacp itself.
- If the binary is missing, use [CLI setup](cli.md#install). If connection is
  refused, check that `qmtcode --acp-ws` is running; do not retry blindly.
- Global flags (`--url`, `--quiet`, `--permission`, `--pretty`) precede the
  subcommand. Permissions default to `allow-once` (tools run); use
  `--permission cancel` to fail closed. Elicitation is always cancelled.
- JSON/NDJSON goes to stdout; connection logs go to stderr. Require exit 0 and
  JSON `ok` not false; `prompt`/`follow` also need a terminal `"type":"done"`.
  Do not use `--pretty` on streams. Acceptance and self idle are not task success.
  `cancel` with `cancellationState: "notification_sent"` confirms only sending the
  notification, not run termination.
- After a timeout or uncertain submission, inspect `runtime` and history before
  retrying, steering, queuing, discarding, or cancelling. Do not claim changes
  succeeded without exit 0 and JSON `ok: true`. Retain receipts and error
  `recovery` IDs before retrying; see [CLI output](cli.md#output).

## Default path

Run from the intended workspace; use its absolute path for later callbacks.

```sh
qmacp --quiet caps
qmacp --quiet prompt --new --cwd "$PWD" --timeout 900 'do the task and report'
```

Keep the returned `sessionId` as `SESSION_ID`. To continue from that workspace:

```sh
qmacp --quiet prompt --cwd "$PWD" --timeout 900 "$SESSION_ID" 'next step'
qmacp --quiet runtime "$SESSION_ID"
```

Long prompts: use stdin (`-`), not nested quoting. Make them self-contained.
Existing-session `prompt` defaults to `--delivery auto`: idle -> prompt,
busy and steerable -> steer, otherwise queue. Auto delivery and idle waits require
the updated backend observation contract; unknown/unavailable is not a usable
phase. Waiting means self-session idle, not descendant/process completion.
Read [live control](control.md) for delivery, waiting, cancellation, and how to
label agent-to-agent messages.

## Read only what you need

| Task | Reference |
|---|---|
| Install, select endpoint/flags, handle output/permissions, or batch commands | [CLI setup and I/O](cli.md) |
| Discover sessions/models/profiles, inspect/configure, manage lifecycle or pins | [Session management](sessions.md) |
| Prompt, follow/watch, steer/queue, discard, cancel, or close | [Live control](control.md) |
| Design a task-specific callback and resume after an external event | [Callbacks and examples](callbacks.md) |
| Use qmacp to validate a running qmtcode candidate | [Testing qmtcode](testing.md) |

References are relative to this skill directory, not the session workspace.
Load only the reference needed at the current boundary. Proactively use callbacks
when they improve the task: move mechanical waiting into a bounded watcher and
continue independent work. The examples are starting points, not a fixed menu;
read the callback reference before launching or handling one.
