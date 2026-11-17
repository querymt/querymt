# Testing qmtcode

qmacp is a protocol client for `qmtcode --acp-ws`. Use it as a harness while
changing qmtcode: start the candidate server, hit it with qmacp, and treat stdout
JSON/NDJSON as the assertion surface. Use ordinary tools, not qmacp sessions, to
edit or test the qmacp repo itself.

Pass `--url` before the subcommand when targeting a non-default candidate endpoint.
Pass `--cwd` to the qmtcode checkout or fixture workspace, not this qmacp repo
unless that is the session under test. Follow the development task's isolation
requirements; do not alter unrelated sessions or user-run services.

## Select only the checks you need

- Handshake: `caps` for protocol version, ACP session features, and QueryMT
  extensions. See [discovery](sessions.md#discover-capabilities-profiles-and-models).
- Discovery: `profiles`, `models` (`--refresh` only for a fresh provider scan).
- Session lifecycle: `new` -> `status`/`runtime`/`inspect` -> `prompt` ->
  `follow`/`watch` -> `cancel`/`close`, only as required by the test.
- Busy-session behavior: prompt a running session with `--delivery auto|steer|queue`;
  confirm `input_state` and `clientInputId`. See [live control](control.md).
- Config RPCs: `set-mode` / `set-model` / `set-effort`, then `status` to verify
  server acceptance. See [session management](sessions.md).
- Permissions: `--permission allow-once|reject-once|cancel`; inspect `permission`
  events. Do not assume fail-closed unless `cancel`. See [CLI policy](cli.md#permissions).
- Elicitation: qmacp always cancels; expect `"type":"elicitation"` and `cancelled: true`.
- Load/list features: if `caps.acp.loadSession`/`list`/`close` are false, do not keep
  calling `inspect`/`sessions`/`close`.

Query `runtime` again for execution state rather than treating earlier output as
current; `status` is configuration only. Auto delivery and waits require updated
backend observation metadata. Unknown/unavailable is not idle or task success.
Require exit 0 and JSON `ok: true` for command success. Cancel's
`cancellationState: "notification_sent"` proves only notification write/flush;
check runtime/outcome evidence separately for termination.
Prompt/follow checks also require a terminal `"type":"done"`, but a receipt or idle
state alone is not semantic proof of the task result. Inspect the relevant result.
