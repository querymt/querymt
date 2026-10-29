-- tool-failure-report.sql
--
-- Read-only tool call failure report over a QueryMT `sessions.db`.
--
-- Parameters (bind with `.parameter set`, see scripts/README.md):
--   :start     inclusive window start, ISO-8601 UTC, e.g. 2026-10-21T00:00:00Z   (required)
--   :end       exclusive window end,   ISO-8601 UTC, e.g. 2026-10-28T00:00:00Z   (required)
--   :provider  optional filter on messages.source_provider of the calling assistant message
--   :model     optional filter on messages.source_model of the calling assistant message
-- An unset :provider / :model means "no filter".
--
-- Windowing is by the timestamp of the assistant message that contains the call
-- (messages.created_at, unix seconds): start <= created_at < end.
--
-- Data model
--   * A call is either a `tool_use` message part, or a `function_call` item inside an
--     `output` message part (item-aware turns store the call only there).
--   * A result is a `tool_result` message part.
--   * Call IDs are NOT unique: providers can reuse an ID for a later call in the same
--     session. A result is therefore paired to a call by (session_id, call_id) and
--     order: the first result after the call and before the next call with the same
--     key. Each call gets at most one result and each result at most one call.
--   * A call without a paired result has outcome `no_result`. It stays in the
--     total-call denominator but is neither a success nor a failure.
--
-- Failure causes (only assigned to failed calls)
--   alias_naming       camelCase/short-form name mismatch on edit, multiedit, replace_symbol
--   stringified_value  array/object argument sent as a JSON string
--   scalar_for_array   scalar sent where an array is expected
--   number_string      string sent where an integer/number is expected
--   unknown_key        additional (undeclared) property rejected
--   other              any other `invalid tool arguments` failure (for example a missing
--                      required field)
--   non_schema         failure that is not an argument validation failure
--
-- Both wordings are classified: the original `is not of type ...` / `is a required
-- property` messages (history before the argument-handling release) and the current
-- `N violation(s): - /path: ...` and `conflicting spellings of ...` messages. A message
-- listing several violations gets one cause: the first rule below that matches.
--
-- The script only reads the database: all working tables are TEMP tables.
-- Run it with `sqlite3 -readonly`.

.print '== parameters =='
SELECT :start AS window_start_inclusive,
       :end AS window_end_exclusive,
       :provider AS provider_filter,
       :model AS model_filter;

DROP TABLE IF EXISTS temp.tfr_calls_all;
DROP TABLE IF EXISTS temp.tfr_results;
DROP TABLE IF EXISTS temp.tfr_paired;
DROP TABLE IF EXISTS temp.tfr_classified;

-- All calls in the database (not window-filtered) so call-ID reuse across the
-- window boundary cannot misattribute a result. seq orders calls and results on one
-- scale: part id * 1000, plus the item index + 1 for calls inside `output` parts.
CREATE TEMP TABLE tfr_calls_all AS
SELECT mp.id * 1000 AS seq,
       m.session_id AS sid,
       m.created_at AS ts,
       m.source_provider AS prov,
       m.source_model AS model,
       json_extract(mp.content_json, '$.data.id') AS call_id,
       json_extract(mp.content_json, '$.data.function.name') AS tool
FROM message_parts mp
JOIN messages m ON m.id = mp.message_id
WHERE mp.part_type = 'tool_use'
UNION ALL
SELECT mp.id * 1000 + CAST(j.key AS INTEGER) + 1,
       m.session_id,
       m.created_at,
       m.source_provider,
       m.source_model,
       json_extract(j.value, '$.call_id'),
       json_extract(j.value, '$.name')
FROM message_parts mp
JOIN messages m ON m.id = mp.message_id,
     json_each(mp.content_json, '$.data.output.items') AS j
WHERE mp.part_type = 'output'
  AND json_extract(j.value, '$.type') = 'function_call';
CREATE INDEX temp.tfr_calls_all_ix ON tfr_calls_all (sid, call_id, seq);

CREATE TEMP TABLE tfr_results AS
SELECT mp.id * 1000 AS seq,
       m.session_id AS sid,
       json_extract(mp.content_json, '$.data.call_id') AS call_id,
       COALESCE(json_extract(mp.content_json, '$.data.is_error'), 0) AS is_error,
       CASE WHEN json_extract(mp.content_json, '$.data.is_error') = 1
            THEN json_extract(mp.content_json, '$.data.content[0].text') END AS err_text
FROM message_parts mp
JOIN messages m ON m.id = mp.message_id
WHERE mp.part_type = 'tool_result';
CREATE INDEX temp.tfr_results_ix ON tfr_results (sid, call_id, seq);

-- One row per in-window call, at most one paired result.
CREATE TEMP TABLE tfr_paired AS
WITH ordered AS (
    SELECT c.*,
           (SELECT min(r.seq) FROM tfr_results r
             WHERE r.sid = c.sid AND r.call_id = c.call_id AND r.seq > c.seq) AS res_seq,
           LEAD(c.seq) OVER (PARTITION BY c.sid, c.call_id ORDER BY c.seq) AS next_seq
    FROM tfr_calls_all c
)
SELECT o.seq, o.sid, o.ts, o.prov, o.model, o.call_id, o.tool,
       r.err_text,
       CASE WHEN r.seq IS NULL THEN 'no_result'
            WHEN r.is_error = 1 THEN 'failed'
            ELSE 'succeeded' END AS outcome
FROM ordered o
LEFT JOIN tfr_results r
       ON r.sid = o.sid AND r.call_id = o.call_id AND r.seq = o.res_seq
      AND (o.next_seq IS NULL OR r.seq < o.next_seq)
WHERE o.ts >= CAST(strftime('%s', :start) AS INTEGER)
  AND o.ts <  CAST(strftime('%s', :end) AS INTEGER)
  AND (:provider IS NULL OR o.prov = :provider)
  AND (:model IS NULL OR o.model = :model);

CREATE TEMP TABLE tfr_classified AS
SELECT p.*,
       CASE
         WHEN outcome <> 'failed' THEN NULL
         WHEN err_text IS NULL OR err_text NOT LIKE 'Error: invalid tool arguments:%' THEN 'non_schema'
         -- Two spellings of one parameter (current wording).
         WHEN err_text LIKE '%conflicting spellings of%' THEN 'alias_naming'
         WHEN tool IN ('edit', 'multiedit', 'replace_symbol') AND (
                -- Old wording: `"newString" is a required property`.
                (err_text LIKE '%is a required property'
                 AND substr(err_text, 33, instr(substr(err_text, 33), '"') - 1) IN
                     ('filePath','oldString','newString','replaceAll','newText','expectedHash',
                      'file_path','old_string','new_string','replace_all','new_text','expected_hash'))
             OR (err_text LIKE '%Additional properties%'
                 AND (err_text LIKE '%filePath%' OR err_text LIKE '%oldString%'
                      OR err_text LIKE '%newString%' OR err_text LIKE '%replaceAll%'
                      OR err_text LIKE '%newText%' OR err_text LIKE '%expectedHash%'
                      OR err_text LIKE '%old_str%' OR err_text LIKE '%new_str%'))
                -- Current wording: an unknown key resembling a declared name.
             OR err_text LIKE '%did you mean%'
             ) THEN 'alias_naming'
         -- Old: `Additional properties ...`; current: "property `x` is not allowed".
         WHEN err_text LIKE '%Additional properties%' OR err_text LIKE '%` is not allowed%'
              THEN 'unknown_key'
         WHEN ((err_text LIKE 'Error: invalid tool arguments: "[%' OR err_text LIKE 'Error: invalid tool arguments: "{%')
               AND (err_text LIKE '%is not of type "array"' OR err_text LIKE '%is not of type "object"'))
              OR err_text LIKE '%expected array, got string: "[%'
              OR err_text LIKE '%expected array, got string: "{%'
              OR err_text LIKE '%expected object, got string:%'
              THEN 'stringified_value'
         WHEN err_text LIKE '%is not of type "array"' OR err_text LIKE '%expected array, got %'
              THEN 'scalar_for_array'
         WHEN err_text LIKE '%is not of type "integer"' OR err_text LIKE '%is not of type "number"'
              OR err_text LIKE '%expected integer, got string:%'
              OR err_text LIKE '%expected number, got string:%'
              THEN 'number_string'
         ELSE 'other'
       END AS cause,
       LAG(outcome) OVER (PARTITION BY sid, tool ORDER BY seq) AS prev_outcome
FROM tfr_paired p;

.print ''
.print '== summary =='
SELECT count(*) AS total_calls,
       sum(outcome = 'succeeded') AS succeeded,
       sum(outcome = 'failed') AS failed,
       sum(outcome = 'no_result') AS no_result,
       sum(cause IS NOT NULL AND cause <> 'non_schema') AS schema_failed,
       ROUND(100.0 * sum(outcome = 'failed') / NULLIF(count(*), 0), 2) AS failure_rate_pct
FROM tfr_classified;

.print ''
.print '== by tool =='
SELECT tool,
       count(*) AS total_calls,
       sum(outcome = 'failed') AS failed,
       ROUND(100.0 * sum(outcome = 'failed') / count(*), 2) AS failure_rate_pct,
       sum(cause IS NOT NULL AND cause <> 'non_schema') AS schema_failed,
       COALESCE(sum(cause = 'alias_naming'), 0) AS alias_naming,
       COALESCE(sum(cause = 'stringified_value'), 0) AS stringified_value,
       COALESCE(sum(cause = 'scalar_for_array'), 0) AS scalar_for_array,
       COALESCE(sum(cause = 'number_string'), 0) AS number_string,
       COALESCE(sum(cause = 'unknown_key'), 0) AS unknown_key,
       COALESCE(sum(cause = 'other'), 0) AS other,
       COALESCE(sum(cause = 'non_schema'), 0) AS non_schema
FROM tfr_classified
GROUP BY tool
ORDER BY failed DESC, total_calls DESC, tool;

.print ''
.print '== failures by tool and cause =='
SELECT tool, cause, count(*) AS failed
FROM tfr_classified
WHERE outcome = 'failed'
GROUP BY tool, cause
ORDER BY failed DESC, tool, cause;

.print ''
.print '== repeated failures (failure directly after a failed call of the same tool in the same session) =='
SELECT tool,
       sum(outcome = 'failed') AS failed,
       sum(outcome = 'failed' AND prev_outcome = 'failed') AS repeated_failures
FROM tfr_classified
GROUP BY tool
HAVING sum(outcome = 'failed' AND prev_outcome = 'failed') > 0
ORDER BY repeated_failures DESC, tool;

.print ''
.print '== repair outcomes =='
.print 'UNAVAILABLE from sessions.db: successful and failed argument repairs are exported as'
.print 'telemetry (querymt.tool.args_repaired and the per-call repaired/outcome attributes),'
.print 'not stored in message history. They are not inferred from the failures above; a repaired'
.print 'call that succeeds leaves no failure row. See scripts/README.md for the telemetry source.'
