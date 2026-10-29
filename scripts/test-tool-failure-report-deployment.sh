#!/usr/bin/env bash
# Pre/post-deployment fixture test for scripts/tool-failure-report.sql.
#
# Builds a synthetic sessions.db-shaped database whose post-release failures use the
# CURRENT validation wording (scripts/testdata/new_format_failures.tsv, regenerated
# from the real pipeline by a Rust test), plus pre-release history in the OLD wording,
# then checks deployment exclusion, three-day vs seven-day windows, denominators,
# cause classification and the "repair outcomes UNAVAILABLE" notice.
#
# Usage: scripts/test-tool-failure-report-deployment.sh   (requires sqlite3 >= 3.38, python3)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPORT="$HERE/tool-failure-report.sql"
TSV="$HERE/testdata/new_format_failures.tsv"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
DB="$TMP/sessions.db"
DEPLOY_ISO="2026-10-28T12:00:00Z"

python3 - "$DB" "$TSV" "$DEPLOY_ISO" "$TMP/times" <<'PY'
import calendar, json, sqlite3, sys, time

db_path, tsv, deploy_iso, times_path = sys.argv[1:5]
deploy = calendar.timegm(time.strptime(deploy_iso, "%Y-%m-%dT%H:%M:%SZ"))
DAY = 86400
db = sqlite3.connect(db_path)
db.executescript("""
CREATE TABLE messages (id INTEGER PRIMARY KEY, session_id INTEGER NOT NULL, role TEXT NOT NULL,
  created_at INTEGER NOT NULL, source_provider TEXT, source_model TEXT);
CREATE TABLE message_parts (id INTEGER PRIMARY KEY, message_id INTEGER NOT NULL,
  part_type TEXT NOT NULL, content_json TEXT NOT NULL, sort_order INTEGER NOT NULL DEFAULT 0);
""")
ids = {"m": 0, "p": 0, "c": 0}

def add(session, ts, tool, text, is_error, provider="anthropic"):
    """One call and, unless text is None, its result one second later."""
    ids["c"] += 1
    call_id = f"call{ids['c']}"
    for role, at, part_type, body in (
        ("Assistant", ts, "tool_use", {"type": "ToolUse", "data": {
            "id": call_id, "type": "function",
            "function": {"name": tool, "arguments": "{}"}}}),
        ("User", ts + 1, "tool_result", {"type": "ToolResult", "data": {
            "call_id": call_id, "is_error": is_error,
            "content": [{"type": "text", "text": text}]}}),
    ):
        if role == "User" and text is None:
            continue
        ids["m"] += 1
        ids["p"] += 1
        db.execute("INSERT INTO messages VALUES (?,?,?,?,?,?)",
                   (ids["m"], session, role, at,
                    provider if role == "Assistant" else None,
                    "claude-x" if role == "Assistant" else None))
        db.execute("INSERT INTO message_parts (id,message_id,part_type,content_json) VALUES (?,?,?,?)",
                   (ids["p"], ids["m"], part_type, json.dumps(body)))

OLD = 'Error: invalid tool arguments: "[{\\"oldString\\":\\"a\\"}]" is not of type "array"'

# Pre-release history (old wording), including the second just before deployment.
add(1, deploy - 5 * DAY, "multiedit", OLD, True)
add(1, deploy - 5 * DAY + 60, "edit", "OK", False)
add(1, deploy - 1, "multiedit", OLD, True)

# Post-release, first three days: every real new-format failure.
t = deploy                                   # inclusive start
for line in open(tsv).read().splitlines():
    tool, _args, message, _cause = line.split("\t")
    add(2, t, tool, message.replace("\\n", "\n"), True)
    t += 600
add(2, deploy + 2 * DAY, "edit", "OK", False)
add(2, deploy + 2 * DAY + 10, "edit", "OK", False)
add(2, deploy + 2 * DAY + 20, "read_tool", "ok", False)
add(2, deploy + 2 * DAY + 30, "shell", "Error: Provider error: exit 1", True)
add(2, deploy + 2 * DAY + 40, "glob", None, False)        # no result recorded
add(2, deploy + 2 * DAY + 50, "multiedit", "x", True, provider="codex")  # other provider

# Days four to seven.
add(3, deploy + 5 * DAY, "edit", "OK", False)
add(3, deploy + 5 * DAY + 10, "multiedit",
    'Error: invalid tool arguments: 1 violation:\n- /edits: expected array, got string: "[{\\"old_string\\": \\"x\\", "; '
    'hint: `edits` must be a JSON array of objects, not a string containing JSON', True)
add(3, deploy + 7 * DAY, "edit", "OK", False)             # exactly at the end: excluded
db.commit()
open(times_path, "w").write("\n".join(str(v) for v in (deploy,)))
PY

run_report() { # <start-iso> <end-iso>
  sqlite3 -readonly -list -separator , -header "$DB" \
    ".parameter init" ".parameter set :start '$1'" ".parameter set :end '$2'" \
    ".parameter set :provider 'anthropic'" ".read $REPORT"
}
iso_plus() { python3 - "$DEPLOY_ISO" "$1" <<'PY'
import calendar, sys, time
t = calendar.timegm(time.strptime(sys.argv[1], "%Y-%m-%dT%H:%M:%SZ")) + int(sys.argv[2])
print(time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(t)))
PY
}

FAILS=0
expect() { # <label> <exact line> <output>
  if grep -qxF -- "$2" <<<"$3"; then echo "ok   - $1"; else
    echo "FAIL - $1: expected line '$2'"; FAILS=$((FAILS + 1)); fi
}
expect_absent() { # <label> <substring> <output>
  if grep -qF -- "$2" <<<"$3"; then echo "FAIL - $1: found '$2'"; FAILS=$((FAILS + 1)); else
    echo "ok   - $1"; fi
}

# columns: total_calls,succeeded,failed,no_result,schema_failed,failure_rate_pct
# by tool: tool,total,failed,rate,schema_failed,alias,stringified,scalar,number,unknown,other,non_schema

# Preliminary three-day window, starting exactly at deployment.
OUT3="$(run_report "$DEPLOY_ISO" "$(iso_plus $((3 * 86400)))")"
expect "3-day summary excludes pre-release and the other provider" "12,3,8,1,7,66.67" "$OUT3"
expect "3-day multiedit: new-format stringified/scalar/unknown" "multiedit,3,3,100.0,3,0,1,1,0,1,0,0" "$OUT3"
expect "3-day edit: conflict=alias_naming, unknown_key, missing field=other, successes in denominator" "edit,5,3,60.0,3,1,0,0,0,1,1,0" "$OUT3"
expect "3-day read_tool: number_string" "read_tool,2,1,50.0,1,0,0,0,1,0,0,0" "$OUT3"
expect "3-day shell failure is non_schema" "shell,1,1,100.0,0,0,0,0,0,0,0,1" "$OUT3"
expect "3-day no-result call stays in the total" "glob,1,0,0.0,0,0,0,0,0,0,0,0" "$OUT3"
expect "repair outcomes reported unavailable" "UNAVAILABLE from sessions.db: successful and failed argument repairs are exported as" "$OUT3"

# Complete seven-day window.
OUT7="$(run_report "$DEPLOY_ISO" "$(iso_plus $((7 * 86400)))")"
expect "7-day summary adds days 4-7, still excludes the call at the end" "14,4,9,1,8,64.29" "$OUT7"
expect "7-day multiedit includes the day-5 stringified failure" "multiedit,4,4,100.0,4,0,2,1,0,1,0,0" "$OUT7"

# Pre-release window contains only old-wording history.
OUTPRE="$(run_report "$(iso_plus -$((86400)))" "$DEPLOY_ISO")"
expect "pre-release window ends exclusive at deployment" "1,0,1,0,1,100.0" "$OUTPRE"
expect "old-wording stringified value still classified" "multiedit,1,1,100.0,1,0,1,0,0,0,0,0" "$OUTPRE"
OUTBASE="$(run_report "$(iso_plus -$((10 * 86400)))" "$DEPLOY_ISO")"
expect "old wording and pre-release success counted together" "3,1,2,0,2,66.67" "$OUTBASE"

# Repair counts must never be inferred from failures.
expect_absent "no repair figures derived from failures" "repaired" "$(grep -v -i 'repairs are exported\|repaired/outcome\|a repaired' <<<"$OUT3" | grep -i -v 'repair outcomes')"

if [ "$FAILS" -ne 0 ]; then echo "$FAILS check(s) failed"; echo "$OUT3"; exit 1; fi
echo "all checks passed"
