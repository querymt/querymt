# Runtime evidence

Use the available evidence to answer a specific question. OpenTelemetry tooling
is optional, not a prerequisite for development or validation.

## Choose the available source

- If a relevant OpenTelemetry skill is available, load it and follow its guidance
  to explore data through an available backend. Let that skill own
  tool-specific commands and setup; do not assume a particular collector or CLI.
- If no suitable skill or backend is available, inspect recorded log files or
  retrieve the needed records directly from the candidate's database read-only.
  Do not install a telemetry stack merely to continue an investigation.
- Correlate evidence with the tested binary/revision, time window and observed
  session/run/input IDs. Retrieve only the relevant records, not full data dumps.

## File logs and database fallback

Use the isolated job's captured logs: the saved journal file on Linux, or the
stdout/stderr files on macOS, as described in [runtime isolation](runtime.md).
Check errors, lifecycle transitions and IDs around the event. If more detail is
needed, adjust the candidate's `RUST_LOG` and rerun a bounded check.

Resolve the actual database path from the candidate's launch/configuration. Inspect
its current schema before selecting session, message, event or other relevant
records; do not assume table names or that every runtime event is persisted. Open
SQLite in read-only mode, use bounded queries, and account for an active WAL. If a
consistent snapshot is needed, use SQLite online backup as described in the
runtime reference rather than copying a live DB file. If the records are in the
user's existing database, obtain a copy through a read-only source connection and
inspect that copy. Never write to the original or stop its service to obtain it.

Logs and persisted records can establish application behavior, but do not prove
OTLP export, complete trace ancestry or original attribute types. Report those
specific gaps if they matter to the change; finish independent checks without
claiming equivalent telemetry coverage.

## When using OpenTelemetry

Follow current QueryMT docs and the loaded skill. Select the OTLP ingest endpoint;
the backend's query endpoint may be different. Keep export
disabled for ordinary lab runs. `QMT_NO_TELEMETRY` disables export when set to any
value, so remove it rather than set it to 0 when testing export. Console logging
and export have separate filters (`RUST_LOG` and `QMT_TELEMETRY_LEVEL`).

Query observed IDs within a bounded time window. Ingestion can lag: retry queries,
not paid prompts. Compare raw records with derived summaries before diagnosing an
application bug. SDK capture tests prove attribute types, omissions and parentage;
backend representations may transform those values. Do not add cache/reasoning
components again to inclusive token totals.
