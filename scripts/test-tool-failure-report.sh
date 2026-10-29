#!/usr/bin/env bash
# Fixture test for scripts/tool-failure-report.sql.
#
# Builds a tiny sessions.db-shaped database (only the `messages` and `message_parts`
# tables the report reads), runs the report read-only and asserts the counts.
#
# Usage: scripts/test-tool-failure-report.sh        (requires sqlite3 >= 3.38)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPORT="$HERE/tool-failure-report.sql"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
DB="$TMP/sessions.db"

START_ISO="2026-10-21T00:00:00Z"
END_ISO="2026-10-28T00:00:00Z"
START=$(date -u -d "$START_ISO" +%s 2>/dev/null || date -u -j -f "%Y-%m-%dT%H:%M:%SZ" "$START_ISO" +%s)
END=$(date -u -d "$END_ISO" +%s 2>/dev/null || date -u -j -f "%Y-%m-%dT%H:%M:%SZ" "$END_ISO" +%s)

SEED="$TMP/seed.sql"
MID=0
PID=0
cat >"$SEED" <<'SQL'
CREATE TABLE messages (id INTEGER PRIMARY KEY, session_id INTEGER NOT NULL, role TEXT NOT NULL,
  created_at INTEGER NOT NULL, source_provider TEXT, source_model TEXT);
CREATE TABLE message_parts (id INTEGER PRIMARY KEY, message_id INTEGER NOT NULL,
  part_type TEXT NOT NULL, content_json TEXT NOT NULL, sort_order INTEGER NOT NULL DEFAULT 0);
SQL

# call <session> <ts> <provider> <model> <call_id> <tool>   -> assistant tool_use part
call() {
  MID=$((MID + 1)); PID=$((PID + 1))
  echo "INSERT INTO messages VALUES ($MID,$1,'Assistant',$2,'$3','$4');" >>"$SEED"
  echo "INSERT INTO message_parts (id,message_id,part_type,content_json) VALUES ($PID,$MID,'tool_use','{\"type\":\"ToolUse\",\"data\":{\"id\":\"$5\",\"type\":\"function\",\"function\":{\"name\":\"$6\",\"arguments\":\"{}\"}}}');" >>"$SEED"
}
# call_output: same, but stored as a function_call item inside an `output` part
call_output() {
  MID=$((MID + 1)); PID=$((PID + 1))
  echo "INSERT INTO messages VALUES ($MID,$1,'Assistant',$2,'$3','$4');" >>"$SEED"
  echo "INSERT INTO message_parts (id,message_id,part_type,content_json) VALUES ($PID,$MID,'output','{\"type\":\"Output\",\"data\":{\"output\":{\"items\":[{\"type\":\"reasoning\",\"summary\":[]},{\"type\":\"function_call\",\"call_id\":\"$5\",\"name\":\"$6\",\"arguments\":\"{}\"}]}}}');" >>"$SEED"
}
# result <session> <ts> <call_id> <is_error 0|1> <text>      -> user tool_result part
result() {
  MID=$((MID + 1)); PID=$((PID + 1))
  local err=false; [ "$4" = 1 ] && err=true
  echo "INSERT INTO messages VALUES ($MID,$1,'User',$2,NULL,NULL);" >>"$SEED"
  echo "INSERT INTO message_parts (id,message_id,part_type,content_json) VALUES ($PID,$MID,'tool_result','{\"type\":\"ToolResult\",\"data\":{\"call_id\":\"$3\",\"content\":[{\"type\":\"text\",\"text\":\"$5\"}],\"is_error\":$err}}');" >>"$SEED"
}

INV='Error: invalid tool arguments: '
M=claude-x

# --- boundaries -------------------------------------------------------------------
# before the window; reuses the call ID of the in-window success below
call   1 $((START - 1)) anthropic $M c_ok read_tool
result 1 $((START - 1)) c_ok 1 "Error: boom"
# exactly at start (inclusive), success
call   1 $START anthropic $M c_ok read_tool
result 1 $((START + 1)) c_ok 0 "ok"
# exactly at end (exclusive): excluded
call   1 $END anthropic $M c_end read_tool
result 1 $((END + 1)) c_end 1 "${INV}\\\"1,\\\" is not of type \\\"integer\\\""
# last second inside the window: stringified edits
call   1 $((END - 1)) anthropic $M c_str multiedit
result 1 $((END - 1)) c_str 1 "${INV}\\\"[{\\\\\\\"oldString\\\\\\\":\\\\\\\"a\\\\\\\"}]\\\" is not of type \\\"array\\\""

# --- duplicate call ID inside the window: two calls, two results --------------------
call   2 $((START + 100)) anthropic $M c_dup read_tool
result 2 $((START + 101)) c_dup 0 "ok"
call   2 $((START + 200)) anthropic $M c_dup read_tool
result 2 $((START + 201)) c_dup 1 "${INV}\\\"466, \\\" is not of type \\\"integer\\\""

# --- call stored only inside an `output` part ---------------------------------------
call_output 3 $((START + 300)) anthropic $M c_out edit
result 3 $((START + 301)) c_out 1 "${INV}\\\"newString\\\" is a required property"

# --- no result, non-schema failure, scalar-for-array, unknown key -------------------
call   3 $((START + 400)) anthropic $M c_none shell
call   3 $((START + 500)) anthropic $M c_shell shell
result 3 $((START + 501)) c_shell 1 "Error: Provider error: exit 1"
call   3 $((START + 600)) anthropic $M c_scalar search_text
result 3 $((START + 601)) c_scalar 1 "${INV}\\\"target/**\\\" is not of type \\\"array\\\""
call   3 $((START + 700)) anthropic $M c_unk edit
result 3 $((START + 701)) c_unk 1 "${INV}Additional properties are not allowed (''replace_everything'' was unexpected)"

# --- other provider/model inside the window -----------------------------------------
call   4 $((START + 800)) codex m1 c_codex glob
result 4 $((START + 801)) c_codex 0 "ok"

sqlite3 "$DB" <"$SEED"

run_report() { # <provider-or-empty> <model-or-empty>
  local args=(".parameter init" ".parameter set :start '$START_ISO'" ".parameter set :end '$END_ISO'")
  [ -n "${1:-}" ] && args+=(".parameter set :provider '$1'")
  [ -n "${2:-}" ] && args+=(".parameter set :model '$2'")
  sqlite3 -readonly -list -separator , -header "$DB" "${args[@]}" ".read $REPORT"
}

FAILS=0
# expect <label> <csv line> <report output>
expect() {
  if grep -qxF -- "$2" <<<"$3"; then echo "ok   - $1"; else
    echo "FAIL - $1: expected line '$2'"; FAILS=$((FAILS + 1)); fi
}

# columns: total_calls,succeeded,failed,no_result,schema_failed,failure_rate_pct
OUT="$(run_report anthropic)"
# in-window anthropic calls: c_ok, c_str, c_dup x2, c_out, c_none, c_shell, c_scalar, c_unk = 9
#   succeeded: c_ok, c_dup#1 = 2 ; failed: c_str, c_dup#2, c_out, c_shell, c_scalar, c_unk = 6 ; no_result: c_none
#   schema failures: all failed except c_shell = 5
expect "provider filter, boundaries and duplicates" "9,2,6,1,5,66.67" "$OUT"
expect "success stays in denominator (read_tool total=3, failed=1)" "read_tool,3,1,33.33,1,0,0,0,1,0,0,0" "$OUT"
expect "output-part call classified as alias_naming" "edit,2,2,100.0,2,1,0,0,0,1,0,0" "$OUT"
expect "stringified multiedit" "multiedit,1,1,100.0,1,0,1,0,0,0,0,0" "$OUT"
expect "scalar for array" "search_text,1,1,100.0,1,0,0,1,0,0,0,0" "$OUT"
expect "shell non_schema failure not counted as schema failure" "shell,2,1,50.0,0,0,0,0,0,0,0,1" "$OUT"

OUT_ALL="$(run_report)"
expect "no filter adds the other provider" "10,3,6,1,5,60.0" "$OUT_ALL"

OUT_MODEL="$(run_report "" m1)"
expect "model filter" "1,1,0,0,0,0.0" "$OUT_MODEL"

# the report must not modify the database
BEFORE="$(sqlite3 "$DB" 'select count(*) from message_parts')"
run_report anthropic >/dev/null
AFTER="$(sqlite3 "$DB" 'select count(*) from message_parts')"
[ "$BEFORE" = "$AFTER" ] && echo "ok   - report is read-only" || { echo "FAIL - report modified the database"; FAILS=$((FAILS + 1)); }

if [ "$FAILS" -ne 0 ]; then echo "$FAILS check(s) failed"; echo "$OUT"; exit 1; fi
echo "all checks passed"
