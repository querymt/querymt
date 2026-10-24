# Session history

## Open the source read-only

Resolve the relevant instance's database: usually `~/.qmt/sessions.db`, unless
`--db`, `QMT_SESSIONS_DB`, `QMT_CONFIG_DIR` or `QMT_HOME` selects another path.
Read it directly with `-readonly`, including while QueryMT is running. Do not add
`immutable=1` to a live database: it can hide WAL data or cause misleading errors.

Examples require Bash and SQLite JSON support. Retain `DB` across commands and
adapt queries to the installed schema.

```bash
DB="$HOME/.qmt/sessions.db" # Replace with the resolved path if different.
test -f "$DB" || { printf 'Database not found: %s\n' "$DB" >&2; exit 1; }
sqlite3 -readonly -bail "$DB" '.timeout 5000' \
  '.schema sessions' '.schema messages' '.schema message_parts'
```

If busy, add `.timeout 5000` to that connection and retry boundedly. Check SQL
stderr and exit status, not just tool-level `is_error`; use `set -o pipefail` when
piping SQLite output.

## Find the session

Validate `<session-prefix>` as a UUID-like prefix; disambiguate matches before
reading content. Use the full public ID thereafter; bind arbitrary search text
rather than interpolating it into SQL.

```bash
sqlite3 -readonly -bail -header -column "$DB" \
  "SELECT id, public_id, name, cwd, created_at, updated_at
   FROM sessions WHERE public_id LIKE '<session-prefix>%'
   ORDER BY updated_at DESC, id DESC LIMIT 50;"
```

For topic lookup, search user prompts or intent summaries across known historical
`cwd` aliases; exclude the current session and verify hits are original work,
not earlier recall requests. Session names alone are insufficient.

`sessions.id` is internal; messages/delegations reference it. `sessions.public_id`
is external; `event_journal.session_id` uses it. Session timestamps are ISO text;
message timestamps are Unix seconds. Page examples by increasing `OFFSET`.

## Read user input

Return all blocks, including steering; distinguish direct requests from injected
instructions or delegated task descriptions. Do not print attachment payloads.

```bash
sqlite3 -readonly -bail -json "$DB" \
  "SELECT m.id, mp.id AS part_id, mp.part_type,
          json_extract(mp.content_json, '$.data.blocks') AS blocks
   FROM messages m JOIN sessions s ON s.id = m.session_id
   JOIN message_parts mp ON mp.message_id = m.id
   WHERE s.public_id = '<session-id>' AND m.role = 'User'
     AND mp.part_type IN ('prompt', 'steering')
   ORDER BY m.id, mp.sort_order, mp.id LIMIT 100 OFFSET 0;"
```

## Extract final replies

Current assistant text lives in `output.items[].parts[]`, not necessarily `text`
parts. Filter the recorded phase rather than calling all assistant text final.

```bash
sqlite3 -readonly -bail -json "$DB" \
  "SELECT m.id, mp.id AS part_id, json_extract(p.value, '$.text') AS text
   FROM messages m JOIN sessions s ON s.id = m.session_id
   JOIN message_parts mp ON mp.message_id = m.id
   JOIN json_each(mp.content_json, '$.data.output.items') i
   JOIN json_each(i.value, '$.parts') p
   WHERE s.public_id = '<session-id>' AND mp.part_type = 'output'
     AND json_extract(i.value, '$.type') = 'message'
     AND json_extract(i.value, '$.phase') = 'final_answer'
     AND json_extract(p.value, '$.type') = 'text'
   ORDER BY m.id, mp.sort_order, mp.id, CAST(i.key AS INTEGER), CAST(p.key AS INTEGER)
   LIMIT 100 OFFSET 0;"
```

Legacy `text` parts use `$.data.content`. If phase is absent, inspect the relevant
text and surrounding results without claiming it is final. Empty extraction is
not evidence of no work; inspect part types and a raw sample before changing paths.

## Pair tool calls with results

Calls can be structured `output` items or legacy `tool_use` parts. Join results
by call ID within the same session, not by message adjacency. A missing result
means unconfirmed execution, not failure or success.

```bash
sqlite3 -readonly -bail -json "$DB" \
  "WITH parts AS (
     SELECT m.id AS message_id, mp.id AS part_id, mp.sort_order,
            mp.part_type, mp.content_json AS j
     FROM messages m JOIN sessions s ON s.id = m.session_id
     JOIN message_parts mp ON mp.message_id = m.id
     WHERE s.public_id = '<session-id>'
   ), calls AS (
     SELECT message_id, part_id, sort_order, CAST(i.key AS INTEGER) AS item_index,
            json_extract(i.value, '$.call_id') AS call_id,
            json_extract(i.value, '$.name') AS name,
            json_extract(i.value, '$.arguments') AS arguments
     FROM parts, json_each(j, '$.data.output.items') i
     WHERE part_type = 'output' AND json_extract(i.value, '$.type') = 'function_call'
     UNION ALL
     SELECT message_id, part_id, sort_order, 0, json_extract(j, '$.data.id'),
            json_extract(j, '$.data.function.name'),
            json_extract(j, '$.data.function.arguments')
     FROM parts WHERE part_type = 'tool_use'
   )
   SELECT c.*, r.message_id AS result_message_id, r.part_id AS result_part_id,
          json_extract(r.j, '$.data') AS result
   FROM calls c LEFT JOIN parts r ON r.part_type = 'tool_result'
     AND json_extract(r.j, '$.data.call_id') = c.call_id
   ORDER BY c.message_id, c.sort_order, c.part_id, c.item_index LIMIT 100 OFFSET 0;"
```

If both call representations occur, deduplicate by call ID before counting.
Arguments and result text may themselves be JSON; parse rather than grep escaped
strings. Inspect `is_error`, command exit status and stderr: wrapper success alone
is not command success. Report session/message/part IDs and any missing evidence.
