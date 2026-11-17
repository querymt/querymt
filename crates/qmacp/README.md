# qmacp

JSON CLI for a running QueryMT ACP WebSocket server (`qmtcode --acp-ws`).

Each invocation connects, does one job, and exits. The server owns session state. Keep the returned `sessionId`. JSON goes to stdout. Connection logs go to stderr.

Default endpoint: `ws://127.0.0.1:3030/acp/ws`.

## Install

Skill, for agents: [`skills/qmacp`](../../skills/qmacp/SKILL.md) in the QueryMT
repository.

```sh
npx skills add querymt/querymt --skill qmacp
```

Binary: download the `qmacp` archive for your platform from the QueryMT
[releases](https://github.com/querymt/querymt/releases), or install it with Nix or Cargo:

```sh
nix profile install github:querymt/querymt#qmacp
cargo install --locked --git https://github.com/querymt/querymt qmacp
```

From a QueryMT checkout:

```sh
cargo install --locked --path crates/qmacp
```

`--version` and `--help` do not connect. `--version` prints `qmacp <crate-version> (<short-git-sha>)`.

## Use

```sh
qmacp --quiet caps
qmacp --quiet prompt --new --cwd . --timeout 900 'fix the build'
qmacp --quiet prompt --cwd . --timeout 900 "$SESSION_ID" 'run the tests'
qmacp --quiet runtime "$SESSION_ID"
qmacp --quiet follow "$SESSION_ID"
```

Globals come before the subcommand: `--url`, `--allow-insecure`, `--pretty`, `--permission`, `--quiet`.

`--permission` defaults to `allow-once`. `reject-once` rejects one tool call. `cancel` selects nothing.

## Commands

Discover: `caps`, `profiles`, `models`

Sessions: `new`, `sessions`, `find`, `inspect`, `status`, `close`

Lifecycle: `get-lifecycle`, `set-lifecycle`; `--resolution any|open|resolved` and `--archive any|excluded|only` filter `sessions` and `find` on the server before pagination. Pins: `pin`, `unpin`; `--pinned` (or `--pinned=false`) filters `sessions` and `find` on the server before pagination. Omitting a filter preserves the existing listing behavior.

Configure: `set-mode` (`build|plan|review`), `set-model`, `set-effort` (`auto|low|medium|high|max`)

Work: `prompt`, `follow`, `watch`, `steer`, `queue`, `discard-queued`, `exec`, `cancel`, `runtime`

`prompt --new TEXT` creates a session. `prompt SESSION_ID TEXT` continues one. `-` reads the prompt from stdin. `--delivery auto` prompts when idle, steers when the turn is steerable, otherwise queues. Auto delivery and idle waits require the updated backend observation metadata; unknown/unavailable status is not usable.

Prompt/follow/watch wait for self-session idle, including queued turns, not for
child workflows or detached processes. Idle is not task success. Idle results
retain runtime observation health and separate direct delegation activity.

`queue SESSION_ID -` and `steer SESSION_ID -` read stdin and return a submission
receipt without loading history. `prompt --delivery queue|steer --until-idle false`
also returns after submission; idle/direct prompt delivery still waits for its turn.

`discard-queued` takes the `inputId` from an `input_state` event. That is the same value as `clientInputId` from the submit response.

`cancel` sends a cancellation notification for the current turn. Its
`cancellationState: "notification_sent"` confirms the local write/flush, not run
termination or task success. It does not discard queued inputs or cancel children.

`find` reports `pagesScanned`, `truncated`, `pageCapReached`, and `stopReason`
so callers can distinguish a complete search from a result/page limit.

Lifecycle updates are explicit desired states, not toggles. At least one field is required; omitted fields remain unchanged. `caps` reports lifecycle feature/method support, and lifecycle commands/filters fail explicitly when the server does not advertise support. Session summaries preserve available lifecycle data in `_meta.querymt.lifecycle` and pin data in `_meta.querymt.pin`, omitting each when unavailable. Pin commands and filters fail explicitly when the server does not advertise pin support.

```sh
qmacp --quiet get-lifecycle "$SESSION_ID"
qmacp --quiet set-lifecycle "$SESSION_ID" --resolved true
qmacp --quiet set-lifecycle "$SESSION_ID" --resolved false --archived true
qmacp --quiet sessions --resolution open --archive excluded
qmacp --quiet find --query 'build' --resolution resolved --archive only
qmacp --quiet pin "$SESSION_ID"
qmacp --quiet unpin "$SESSION_ID"
qmacp --quiet sessions --pinned
qmacp --quiet find --query 'build' --pinned=false
```

`exec` runs several commands on one connection. It is not a shell. Lifecycle and pin commands are supported in `exec` too:

```sh
qmacp --quiet exec <<EOF
get-lifecycle $SESSION_ID
set-lifecycle $SESSION_ID --resolved false
sessions --resolution open --archive excluded
pin $SESSION_ID
sessions --pinned
EOF
```

## Output

Most commands, including `new`, print one JSON object. `prompt`, `follow`, and
`exec` stream NDJSON. Opt in to `new --stream` for an early creation receipt and
a final `type: done` result. `watch` also emits `runtime` events.

Exit codes: 0 ok, 1 usage, 2 connection, 3 RPC, 4 timeout or cancel.

Event types: `session`, `submitted`, `text`, `tool`, `delegation`, `mode`, `plan`, `input_state`, `permission`, `elicitation`, `runtime`, `update`, `done`.

`delegation` records describe workflow lifecycle separately from dispatch `tool`
status. Compact `inspect` includes snapshot `delegations`; successful dispatch
alone does not establish delegation success.

Submission receipts retain `clientInputId`; partial-failure errors include known
IDs and delivery state in `recovery`. Acceptance is not task completion. See
[CLI output and recovery](skills/qmacp/cli.md#output) for details.

Elicitation is cancelled and emitted as an event.

## Library

The `qmacp` crate also exports its native ACP client and shared session helpers.
Embedded callers can receive compact events without writing to stdout:

```rust,no_run
use qmacp::{client::AcpClient, policy::PermissionPolicy};

# async fn example() -> anyhow::Result<()> {
let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(64);
let events = tokio::spawn(async move {
    while let Some(event) = events_rx.recv().await {
        // Forward or collect events; do not write them to an MCP stdout stream.
        drop(event);
    }
});
let client = AcpClient::connect_with_events(
    "ws://127.0.0.1:3030/acp/ws",
    "127.0.0.1:3030/acp/ws",
    PermissionPolicy::Cancel,
    events_tx,
).await?;
let initialized = client.initialize().await?;
assert_eq!(initialized.protocol_version, qmacp::protocol::schema::ProtocolVersion::V1);
drop(client);
events.await?;
# Ok(())
# }
```

The caller must continuously drain the bounded event receiver while operations
are running. Library-mode assistant text is limited to 1 MiB and reports
truncation through `text_truncated()`. `connect(..., stream_events)` retains the
CLI event sink and unbounded assistant text behavior. The CLI and library use
the same implementation of WebSocket RPCs, permissions, and event parsing.

Dropping a client aborts its reader/writer tasks and releases the connection;
it does not cancel remote work or close the remote session. Explicit
`cancel(session_id).await` waits until its ACP notification has been written,
not until the remote turn has stopped. URL normalization and endpoint redaction
are available in `qmacp::url`, and ACP schema types in `qmacp::protocol`.

Secure WebSockets use native trust roots and an explicit connection-local ring
provider. The library does not install or replace rustls's process-wide crypto
provider, even when an embedding application also enables AWS-LC.
