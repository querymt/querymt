# Delegation recovery

The ID in the parent's failure message is `delegations.public_id`, not a session
ID. Follow [session history](session-db.md) to open the source read-only and reuse
`DB`. Validate the full `<delegation-id>` and inspect the installed schema:

```bash
sqlite3 -readonly -bail "$DB" '.schema delegations' '.schema event_journal'
```

## Recover the specification and error

```bash
sqlite3 -readonly -bail -json "$DB" \
  "SELECT d.public_id, parent.public_id AS parent_session_id,
          d.target_agent_id, d.status, d.retry_count, d.created_at,
          d.objective, d.context, d.constraints, d.expected_output,
          d.verification_spec, d.planning_summary, d.isolation
   FROM delegations d JOIN sessions parent ON parent.id = d.session_id
   WHERE d.public_id = '<delegation-id>';"
```

Match structured IDs, not substrings of serialized payloads. Keep errors in event
order: different attempts may fail differently.

```bash
sqlite3 -readonly -bail -json "$DB" \
  "SELECT e.stream_seq, datetime(e.timestamp, 'unixepoch') AS created_utc,
          json_extract(e.payload_json, '$.data.error') AS error
   FROM event_journal e
   JOIN delegations d ON d.public_id = '<delegation-id>'
   JOIN sessions parent ON parent.id = d.session_id
   WHERE e.session_id = parent.public_id AND e.kind = 'delegation_failed'
     AND json_extract(e.payload_json, '$.data.delegation_id') = d.public_id
   ORDER BY e.stream_seq LIMIT 50 OFFSET 0;"
```

If records are absent, confirm the ID and database instance; report gaps rather
than inferring an exact error from status. Check relevant logs if available.

## Find the delegate session

`session_forked` links the delegation to its child through `fork_point_ref`.
Retrieve all attempts and correlate event order with failures.

```bash
sqlite3 -readonly -bail -json "$DB" \
  "SELECT e.stream_seq, datetime(e.timestamp, 'unixepoch') AS created_utc,
          json_extract(e.payload_json, '$.data.child_session_id') AS child_session_id,
          json_extract(e.payload_json, '$.data.target_agent_id') AS target_agent_id,
          child.id AS local_session_id, child.cwd
   FROM event_journal e
   JOIN delegations d ON d.public_id = '<delegation-id>'
   JOIN sessions parent ON parent.id = d.session_id
   LEFT JOIN sessions child
     ON child.public_id = json_extract(e.payload_json, '$.data.child_session_id')
   WHERE e.session_id = parent.public_id AND e.kind = 'session_forked'
     AND json_extract(e.payload_json, '$.data.fork_point_ref') = d.public_id
   ORDER BY e.stream_seq LIMIT 50 OFFSET 0;"
```

A missing local child may belong to another instance/peer; consult its history
only when available and authorized. Failure before session creation leaves none.
If the event is missing, inspect `sessions` children sharing the parent's internal
ID with `fork_origin = 'delegation'` and their fork metadata. Parentage, timestamps
and UUID prefixes alone do not establish a match; report ambiguity.

## Recover partial work and decide

For a confirmed child, use [call/result pairing](session-db.md#pair-tool-calls-with-results)
to reconstruct what actually completed, including the last result after the last
assistant call. Check subsequent attempts before assuming work stopped there.

- Missing files may be recoverable from successful `write_file` arguments plus
  subsequent successful edits in order. Extract only relevant content; never
  blindly replay commands or overwrite current files. Compare actual files,
  receipts and test evidence before treating recovered work as reusable.
- Check for active attempts and already-applied side effects before resubmitting.
  Fix the cause within the authorized task, preserve useful work, and retain the
  original objective, constraints, output, verification and isolation requirements.
  Retry through normal delegation tools, never database edits.
- Report delegation/parent/child IDs, the exact error, confirmed versus attempted
  work and the next safe action. Keep retries bounded; ask before repeating risky
  side effects, exceeding authorization or continuing repeated failures.
