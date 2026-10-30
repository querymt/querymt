# Notes

## Release note (task 7.4)

Mesh peers need this change on **both** sides for ordered delivery of relayed
provider streams. The host numbers data, terminal and stream-failure relay
messages per request; the client-side router delivers them in order, discards
duplicates, and fails the stream with a retryable transport error
(`relay sequence gap: missing seq N (held M)`) if a gap is not filled within
15 s or more than 1024 messages are held.

- Ship with or before `structured-anthropic-streaming`, which moves Anthropic
  onto the strict accumulator that rejects reordered streams.
- Mixed versions keep working: `seq` is an optional, defaulted field. An old
  host gives unsequenced messages (delivered as received); a new host sent to
  an old router has its `seq` ignored.
- Optional interim for mixed deployments where only the host can be upgraded:
  make `should_ack_relay_message` return `true` for data messages, which
  serializes mailbox entry at the cost of one round trip per batch. Revert
  once both peers have sequencing.
- The router-to-client channel is now unbounded; a slow client no longer
  loses or reorders messages.

The PR description must carry this note (not done here: no PR exists yet).

## Live mesh check (task 7.3)

Done (Anthropic only; see the codex follow-up below). Session
`01a18993-07fc-7596-89c4-e99aaadcbb66`, cwd `/home/viktor/mlbh` on `dagon`,
Anthropic `claude-opus-5-5` served over the mesh from another node, both
peers on this change. `dagon` runs the session, so it is the client: the
router (and its logs) live there. Sources: the session's `event_journal`
rows in `~/.qmt/sessions.db`, and `dagon`'s router log.

### Journal: before vs after

| | Before (2026-10-29 21:31–21:53 UTC) | After (2026-10-30 08:58–09:06 UTC, still running) |
|---|---|---|
| LLM attempts | 50 | 19, all settled |
| `stream_accumulation` retries | 4 | 0 |
| Errors | 1 (`unfinished output indexes: [1]`, run failed) | 0 |
| Tool calls | 56 | 25 |

Before the change, the journal showed `received data after completion` and
`unfinished output indexes`, the errors this change targets. None have
appeared since.

### Router log counts (2026-10-30 09:13–09:18 UTC)

From `dagon` run with `RUST_LOG=querymt_remote::provider::router=debug`.
Counted by hand from the pasted log.

| Request | Held out of order | Max held at once | Duplicates | Gap failures | Terminal |
|---|---|---|---|---|---|
| `…3908` | 0 | 0 | 0 | 0 | completed |
| `…a367` | 0 | 0 | 0 | 0 | completed |
| `…f339` (long, ~155 messages) | 59 | 5 | 0 | 0 | completed |
| `…bb67` | 9 | 4 | 0 | 0 | completed |
| `…765d` | 2 | 2 | 0 | 0 | completed |
| `…a7e4` | 6 | 3 | 0 | 0 | completed |
| `…27ca` | 0 | 0 | 0 | 0 | completed |
| `…513b` | 2 | 2 | 0 | 0 | completed |
| **Total** | **78** | **5** | **0** | **0** | **8 of 8** |

The journal agrees for the same window: 8 LLM attempts, all settled, no
errors, no `stream_accumulation`.

### Conclusions

- Reordering on the mesh is routine, not rare: 5 of 8 requests had held
  messages, in fast bursts. This confirms the diagnosis.
- Every gap closed within about 1 ms (e.g. seq 8 held at 09:16:19.086421,
  released by 09:16:19.087369). The 15 s gap timeout has ample headroom and
  stays as is.
- The 1024 held-message cap is far from the observed maximum of 5.
- No duplicates were seen, as expected without reconnects. The duplicate
  and reconnect paths are covered by tests only, not live.
- The per-message "held out-of-order" line stays at `debug`; its volume
  (78 lines in 5 minutes) is acceptable at that level.

### Follow-up

- **Codex should eventually be tested** over the mesh the same way. Only
  Anthropic turns were run for this check.

## Clippy (task 7.2)

The literal workspace command cannot finish on this machine: `pyo3-ffi`
fails in its build script, and `querymt-remote/examples/share_openai.rs`
needs the `kameo-mesh` feature. Both are unrelated to this change. Instead,
`cargo clippy -p querymt-remote --all-targets` was compared with and without
this change (identical 7 warnings, none in touched files), and
`querymt-agent --features remote` shows no warnings in touched files.

## Task 6.3 status

Partly covered, so left unchecked. `RelayQueue` tests show a resend re-reads
the same `seq`, and a router test shows a duplicate `seq` is delivered once.
There is no test that forces a real send failure inside the host relay loop
and then reconnects; that needs a router ref that can be made to fail.
