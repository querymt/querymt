# scripts

## Tool failure report

`tool-failure-report.sql` reports tool-call volume, failures, failure rates and
failure causes from a QueryMT `sessions.db`. It only reads the database (all working
tables are `TEMP` tables) and runs in about two seconds on a 2.8 GB database.

### Running it

Run from the repository root. Always open the database `-readonly`, and bound the run
with a timeout so a mistake cannot run unattended:

```sh
perl -e 'alarm 120; exec @ARGV' sqlite3 -readonly -header -column ~/.qmt/sessions.db \
  ".parameter init" \
  ".parameter set :start \"'2026-10-21T00:00:00Z'\"" \
  ".parameter set :end \"'2026-10-28T00:00:00Z'\"" \
  ".parameter set :provider \"'anthropic'\"" \
  ".read scripts/tool-failure-report.sql"
```

| Parameter   | Meaning                                                               |
|-------------|-----------------------------------------------------------------------|
| `:start`    | Inclusive window start, ISO-8601 UTC. Required.                       |
| `:end`      | Exclusive window end, ISO-8601 UTC. Required.                         |
| `:provider` | Optional. Matches `messages.source_provider` of the calling message.  |
| `:model`    | Optional. Matches `messages.source_model` of the calling message.     |

Leave `:provider` / `:model` unset (omit the `.parameter set` line) for no filter.
The window applies to the timestamp of the assistant message that made the call.

Sections printed: parameters, summary, by tool, failures by tool and cause, repeated
failures, and repair outcomes (see below).

`scripts/test-tool-failure-report.sh` checks the report against a small fixture
database (window boundaries, duplicate call IDs, calls stored in `output` parts, calls
without a result, provider/model filters, read-only behaviour):

```sh
scripts/test-tool-failure-report.sh
```

### How calls and results are paired

- A call is a `tool_use` message part, or a `function_call` item inside an `output`
  part (item-aware turns store the call only there). Both are counted.
- Call IDs are not unique: an ID can be reused for a later call in the same session.
  A result is paired by `(session_id, call_id)` with the first result after the call and
  before the next call with the same key. Each call has at most one result.
- A call without a result is reported as `no_result`. It counts in the total but is
  neither a success nor a failure.
- `failure_rate_pct` is failed calls divided by all calls in the window, so successes
  stay in the denominator.

### Failure causes

`alias_naming`, `stringified_value`, `scalar_for_array`, `number_string`,
`unknown_key`, `other` (all `invalid tool arguments` failures) and `non_schema`
(any other failed call). `schema_failed` is the sum of the first six. Causes are
classified from the stored error text, so they depend on the validation message format;
the format changes with this change's section 5, and the `other` bucket will grow if
new messages are not added to the classifier.

### Snapshots and reproducibility

To get repeatable numbers, copy the database once and query the copy. A copy keeps the
WAL journal mode, so open it with `immutable=1` (a plain `-readonly` open fails without
`-wal`/`-shm` files):

```sh
sqlite3 -readonly ~/.qmt/sessions.db ".backup /tmp/sessions-snapshot.db"
shasum -a 256 /tmp/sessions-snapshot.db
sqlite3 -readonly -header -column "file:/tmp/sessions-snapshot.db?immutable=1" \
  ".parameter init" \
  ".parameter set :start \"'2026-10-21T00:00:00Z'\"" \
  ".parameter set :end \"'2026-10-28T00:00:00Z'\"" \
  ".parameter set :provider \"'anthropic'\"" \
  ".read scripts/tool-failure-report.sql"
```

Record the snapshot's SHA-256 with any numbers you report.

### Baseline (provenance)

Window `[2026-10-21T00:00:00Z, 2026-10-28T00:00:00Z)`, provider `anthropic`, no model
filter. Snapshot taken 2026-10-28; SHA-256 of the snapshot file begins `3d610356df3cd681`
(the complete hash is `3d610356df3cd68181fac5b1d283954788202594aec404f912a22a24b71da4e7`).
The latest message in the snapshot is 2026-10-28 01:00:45 UTC. Two runs on the snapshot
produced byte-identical output.

| Measure                              | Value            |
|--------------------------------------|------------------|
| Total tool calls                     | 9,324            |
| Succeeded                            | 9,172            |
| Failed                               | 151 (1.62 %)     |
| No result recorded                   | 1                |
| Schema (`invalid tool arguments`)    | 111              |

Schema failures by tool and cause, against the figures quoted in the proposal
(108 schema failures: multiedit 57, search_text 25, edit 19, read_tool 3,
replace_symbol 3, question 1):

| Tool           | Report | Proposal | Report cause breakdown                          |
|----------------|--------|----------|-------------------------------------------------|
| multiedit      | 59     | 57       | 58 `stringified_value`, 1 `other` (missing `edits`) |
| search_text    | 26     | 25       | 26 `scalar_for_array`                           |
| edit           | 20     | 19       | 20 `alias_naming` (missing `newString`)         |
| read_tool      | 3      | 3        | 3 `number_string`                               |
| replace_symbol | 3      | 3        | 3 `other` (missing `replacements`)              |
| question       | 0      | 1        | -                                               |
| Total          | 111    | 108      |                                                 |

Differences are reported, not forced to agree. What is known about them:

- Totals of 151 failed calls match the 151 in the proposal.
- The report counts +2 multiedit, +1 search_text and +1 edit, and 0 `question` schema
  failures against 1. Across all providers there are 3 `question` header-length
  failures in the window, but none belong to a call attributed to `anthropic`, so the
  report excludes them. The reason for the remaining differences has not been
  established; the proposal's per-tool query is not preserved, so it cannot be compared
  directly.
- Repeated failures (a failure directly after a failed call of the same tool in the
  same session): multiedit 16, edit 3, replace_symbol 2, search_text 1.

### Repair outcomes (separate telemetry source)

Argument repairs and their outcomes are not stored in `sessions.db`: a repaired call
that succeeds leaves no failure record, so they cannot be derived from the failures
above. The report prints this as **UNAVAILABLE** and never infers it.

The source is exported telemetry (see `docs/docs/telemetry.md`):
`querymt.tool.args_repaired{tool, rule}` and a per-call repaired / execution-outcome
indicator, added by the argument-normalisation work (task 4.5 of
`harden-tool-argument-handling`). Until that ships and a metrics backend is receiving
it, repair figures are unavailable. Backend queries and retention are documented with
that telemetry.

### Deployment boundaries and observation windows

- The baseline above is pre-release: it contains no calls handled by the new
  normalisation.
- A post-release report must use a window that begins after the release is deployed to
  that installation, so it contains no pre-release calls. Pass the deployment time as
  `:start`.
- Report a preliminary three-day window and a complete seven-day window for each
  deployment, with the same `:provider`, and compare failure rates and call volumes
  (not absolute failure counts) with the baseline.
- Production improvement is an observation, not an acceptance gate for the
  implementation.

#### Post-release commands

Set `DEPLOY` to the UTC time the release reached this installation (the first
instant that can contain a call handled by the new normalisation). Run on a snapshot
(see above) so both windows read the same data:

```sh
DEPLOY=2026-11-04T09:00:00Z            # example; use the real deployment time
SNAP="file:/tmp/sessions-snapshot.db?immutable=1"
report() {  # <end-iso>
  sqlite3 -readonly -header -column "$SNAP" \
    ".parameter init" \
    ".parameter set :start \"'$DEPLOY'\"" \
    ".parameter set :end \"'$1'\"" \
    ".parameter set :provider \"'anthropic'\"" \
    ".read scripts/tool-failure-report.sql"
}
# Preliminary: three days after deployment (preliminary only)
report "$(date -u -j -v+3d -f %Y-%m-%dT%H:%M:%SZ "$DEPLOY" +%Y-%m-%dT%H:%M:%SZ)"   # macOS
# Complete: seven days after deployment
report "$(date -u -j -v+7d -f %Y-%m-%dT%H:%M:%SZ "$DEPLOY" +%Y-%m-%dT%H:%M:%SZ)"
# GNU date: date -u -d "$DEPLOY + 7 days" +%Y-%m-%dT%H:%M:%SZ
```

Do not report the seven-day figures until the window has fully elapsed. Compare
`failure_rate_pct` and `total_calls` with the baseline table above, not absolute
failure counts.

Repair outcomes come from the metrics backend, over the same window:
`querymt.tool.calls{repaired="true", outcome=...}` for successful versus failed
repaired calls and `querymt.tool.args_repaired{tool, rule}` for rules (queries in
`docs/docs/telemetry.md`). With [otelite](https://github.com/planetf1/otelite) the
data is served at `GET /api/metrics?limit=N` (rows carry `name`, `value`, `attributes`
and `resource`); `GET /api/metrics/names` was empty in the version checked, so list
metrics through `/api/metrics`. If the backend has no data for the window, report repair
outcomes as unavailable.

#### Verifying the report and the telemetry

- `scripts/test-tool-failure-report.sh`: window boundaries, filters, duplicate call IDs
  and read-only behaviour, with the original wording.
- `scripts/test-tool-failure-report-deployment.sh`: a synthetic pre/post deployment
  database. It checks that the deployment instant is inclusive and earlier calls are
  excluded, three-day and seven-day windows, success counts in the denominator, a call
  without a result, another provider's calls, classification of both old and current
  messages, and the repair-outcomes UNAVAILABLE notice. Its post-release failure
  messages are `scripts/testdata/new_format_failures.tsv`; the Rust test
  `report_fixture_messages_match_the_real_pipeline` fails when the validator wording
  drifts from it, and `REGEN_REPORT_FIXTURES=<path>` regenerates the file.
- Export check against a real collector (ignored by default):

  ```sh
  QMT_TEST_OTLP_ENDPOINT=http://127.0.0.1:4317 \
    cargo nextest run -p querymt-agent --lib --run-ignored only otlp_export
  curl -s 'http://localhost:3000/api/metrics?limit=200'   # service.name = qmt-repair-check
  ```

  On 2026-10-28 this produced in otelite `querymt.tool.args_repaired{tool, rule="alias"}`
  = 3 and `querymt.tool.calls` for each of repaired/clean × success/rejected = 1
  (the rejected clean call is the hook block). This proves export and attribute shape;
  it does not show real model traffic.
