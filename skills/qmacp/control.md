# Live control

Use this reference to prompt or observe a session and control active/queued work.
Use the exact `SESSION_ID` returned by creation or selected through
[session discovery](sessions.md); run from that session's workspace.
Global flags precede subcommands; [CLI setup](cli.md) covers endpoints,
permissions (default `allow-once`), and output validation.

## Prompt

```sh
qmacp --quiet prompt --new --cwd "$PWD" --profile '<profile>' --mode build --model '<model-id>' --effort high --timeout 900 'text'
qmacp --quiet prompt --cwd "$PWD" --delivery auto --timeout 900 "$SESSION_ID" 'text'
qmacp --quiet prompt --cwd "$PWD" --delivery queue --until-idle false "$SESSION_ID" 'text'
qmacp --quiet --permission allow-once prompt --new --cwd "$PWD" --timeout 900 - < instructions.txt
```

Discover profile/model values rather than copying placeholders. Use stdin (`-`)
for long payloads; queue/steer require text or an explicit `-`. Make prompts
self-contained; elicitation is always cancelled. Existing sessions accept
`--delivery auto|prompt|steer|queue`:

- `auto` (default): idle -> prompt, busy and steerable -> steer, otherwise queue.
- `prompt`: send a prompt directly; do not use to bypass a busy session's state.
- `steer`: adjust active work. Direct `steer --run-id` can target an exact run.
- `queue`: submit input for after the current turn. Whether it starts an idle
  session depends on server support; use `auto` when that support is unknown.

`--until-idle` defaults true and waits for this session's own idle state, including
queued turns. It does not wait for descendant workflows or detached OS processes,
and idle is not task success. Pass `--timeout` explicitly for predictable bounds.
`--until-idle false` skips waiting for idle following steer/queue; it is not
fire-and-forget when idle delivery uses the prompt RPC, which still waits for
that turn.

Queue/steer delivery skips history loading. Direct commands return a receipt;
`prompt --until-idle false` returns after submission when either is selected.
See [CLI output](cli.md#output) for receipt and observation fields.

## Agent-to-agent messages

For agent-authored handoffs, updates, and requests (not callbacks), use a concise
Markdown body and append this blockquote as the final line:

> Sent by an AI agent, not a human.

Do not add frontmatter by default; reserve it for workflows that need agreed
structured fields. For callbacks, use the
[frontmatter format](callbacks.md#deliver-a-callback).
Neither the footer nor frontmatter authenticates the sender or grants authority
beyond the existing task.

## Observe and control

```sh
qmacp --quiet runtime "$SESSION_ID"
qmacp --quiet follow "$SESSION_ID" --timeout 900 --interval 1
qmacp --quiet watch "$SESSION_ID" --timeout 900 --interval 1 --idle true
qmacp --quiet steer "$SESSION_ID" --run-id "$RUN_ID" 'adjust the active work'
qmacp --quiet queue "$SESSION_ID" 'do this after the current turn'
qmacp --quiet queue "$CALLBACK_SESSION_ID" - < callback.txt
qmacp --quiet discard-queued "$SESSION_ID" "$INPUT_ID"
qmacp --quiet cancel "$SESSION_ID"
qmacp --quiet close "$SESSION_ID"
```

- `follow` requires successful session open, then streams events until self idle;
  run it from the session workspace (cwd comes from the current process).
- `watch` polls `runtime` (`"type":"runtime"`) until self idle unless `--idle false`.
  Default `follow`/`watch` timeout is 120s. Unknown/unavailable observation fails
  rather than completing either command, including nonwaiting watch.
- Auto delivery and waits require the updated backend `observation.availability`
  contract (`live` or known `inactive`); a bare phase is not usable evidence.
  `runtime.delegation_activity` is separate direct-workflow activity and may remain
  live after self idle. Prompt/follow idle results retain the full `runtime`.
  Live `delegation` events report workflow lifecycle separately from dispatch
  `tool` events; neither changes follow's self-idle completion scope.
- `steer --run-id` is optional; if omitted, qmacp pins `runtime.active_run_id`.
  Missing/empty run IDs fail before submission rather than steering unguarded.
- Queued submit returns `clientInputId`. `input_state` events expose the same ID
  as `inputId`; pass that exact value to `discard-queued`.
- `cancel` returns `cancellationState: "notification_sent"` after the local
  WebSocket write/flush. It does not confirm backend acceptance or run termination,
  discard queued inputs, or cancel child workflows. Inspect `runtime` separately;
  a queued next run can keep the session busy.
- Do not close a session unless the user asked to end it or a qmtcode test
  explicitly requires `close`.

After a timeout, inspect `runtime` and session history before resending or
changing work. Do not steer, queue, discard, or cancel just because time ran out.
Require exit 0 and JSON `ok` not false; prompt/follow also need a terminal
`"type":"done"`. For cancel, exit 0 and JSON `ok: true` prove only notification
sending; for discard/close they confirm the respective operation. A submission
receipt is not proof the agent completed the task.

For event-driven waiting outside the agent turn, see [callbacks](callbacks.md).
