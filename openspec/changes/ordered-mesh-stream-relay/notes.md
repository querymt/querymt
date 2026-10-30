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

Not run. Requires two peers (this machine and `dagon`) both on this change.
Record here, once run: `relay sequence gap` failure count, held-message count
and duplicate count from the `querymt_remote::provider::router` debug logs,
plus confirmation of no `stream_accumulation` errors in the journal.
Gap timeout (15 s) is an untuned choice; use these counts to tune it.

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
