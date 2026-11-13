#!/usr/bin/env bash
# Repository-knowledge A/B measurement: the same prompts with the feature on
# and off, repeated, summarised from the sessions databases.
#
# Usage: scripts/kb-measure.sh [-n REPEATS] [-p PROMPTS_FILE] [-o OUT_DIR] [-m MAX_PROMPTS]
#
#   PROMPTS_FILE  one prompt per line; blank lines and lines starting with # are skipped
#                 (default: scripts/kb-measure-prompts.txt)
#   OUT_DIR       per-run databases, logs and results.csv (default: /tmp/kb-measure)
#
# Needs provider credentials for the model named in
# crates/agent/examples/confs/repository_knowledge_coder.toml. Every run calls
# the model: cost is roughly (prompts x repeats x 2) x $0.15.
#
# Each run uses its own sessions database (QMT_SESSIONS_DB), so metrics need no
# session lookup. The index cache is warmed once before the first run, so this
# measures the warm-cache case; delete ~/.qmt/cache/knowledge to test cold.
# Runs alternate between arms (on, off, on, off...) so drift in provider
# latency or cache state does not favour one arm.
#
# This measures cost, requests and context, not answer quality. Final answers
# are kept in OUT_DIR/*.log for reading side by side.
set -euo pipefail

REPEATS=3
MAX_PROMPTS=0
OUT=/tmp/kb-measure
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROMPTS="$ROOT/scripts/kb-measure-prompts.txt"
while getopts "n:p:o:m:" opt; do
  case $opt in
    n) REPEATS=$OPTARG ;;
    p) PROMPTS=$OPTARG ;;
    o) OUT=$OPTARG ;;
    m) MAX_PROMPTS=$OPTARG ;;
    *) sed -n '2,22p' "$0"; exit 2 ;;
  esac
done

CONF_DIR="$ROOT/crates/agent/examples/confs"
BASE_CONF="$CONF_DIR/repository_knowledge_coder.toml"
# Copies live beside the original: the config references ../prompts/* relatively.
ON_CONF="$CONF_DIR/.kb-measure-on.toml"
OFF_CONF="$CONF_DIR/.kb-measure-off.toml"
trap 'rm -f "$ON_CONF" "$OFF_CONF"' EXIT

# Force the arm regardless of what the checked-in example says.
sed -E 's/^enabled = (true|false).*$/enabled = true/' "$BASE_CONF" > "$ON_CONF"
sed -E 's/^enabled = (true|false).*$/enabled = false/' "$BASE_CONF" > "$OFF_CONF"
grep -q '^enabled = true$' "$ON_CONF" || { echo "no 'enabled =' line in $BASE_CONF" >&2; exit 1; }

mkdir -p "$OUT"
CSV="$OUT/results.csv"
echo "prompt_id,arm,rep,requests,tool_calls,cost_usd,peak_context,cards" > "$CSV"

cd "$ROOT"
echo "building..." >&2
cargo build -q -p querymt-agent --example from_config --example repository_knowledge 2>/dev/null
BIN="$ROOT/target/debug/examples/from_config"
echo "warming the index cache..." >&2
"$ROOT/target/debug/examples/repository_knowledge" --root . refresh >/dev/null 2>&1 || true

# No `mapfile`: macOS ships bash 3.2.
LINES=()
while IFS= read -r line; do LINES+=("$line"); done < <(grep -v -E '^\s*(#|$)' "$PROMPTS")
if [ "$MAX_PROMPTS" -gt 0 ]; then LINES=("${LINES[@]:0:$MAX_PROMPTS}"); fi

metric() { sqlite3 "file:$1?mode=ro" "$2" 2>/dev/null || echo ""; }

run_one() { # id arm rep prompt
  local id=$1 arm=$2 rep=$3 prompt=$4 conf db log
  conf=$ON_CONF; [ "$arm" = off ] && conf=$OFF_CONF
  db="$OUT/$id-$arm-$rep.db"; log="$OUT/$id-$arm-$rep.log"
  rm -f "$db" "$db-wal" "$db-shm"
  printf '%s\nquit\n' "$prompt" \
    | QMT_SESSIONS_DB="$db" "$BIN" "${conf#$ROOT/}" >"$log" 2>&1 || true
  local req tools cost ctx cards
  req=$(metric "$db" "select count(*) from event_journal where kind='llm_request_end'")
  tools=$(metric "$db" "select count(*) from event_journal where kind='tool_call_end'")
  cost=$(metric "$db" "select ifnull(max(json_extract(payload_json,'\$.data.cumulative_cost_usd')),0) from event_journal where kind='llm_request_end'")
  ctx=$(metric "$db" "select ifnull(max(json_extract(payload_json,'\$.data.context_tokens')),0) from event_journal where kind='llm_request_end'")
  cards=$(metric "$db" "select count(*) from message_parts where part_type='hook_context' and content_json like '%repository_knowledge%'")
  echo "$id,$arm,$rep,${req:-0},${tools:-0},${cost:-0},${ctx:-0},${cards:-0}" | tee -a "$CSV" >&2
}

idx=0
for line in "${LINES[@]}"; do
  idx=$((idx + 1)); id=$(printf 'p%02d' "$idx")
  for rep in $(seq 1 "$REPEATS"); do
    # Alternate which arm goes first.
    if [ $((rep % 2)) -eq 1 ]; then order="on off"; else order="off on"; fi
    for arm in $order; do run_one "$id" "$arm" "$rep" "$line"; done
  done
done

echo >&2
echo "== mean per arm (all prompts) ==" >&2
sqlite3 -header -column :memory: <<SQL >&2
.mode csv
.import '$CSV' r
.mode column
select arm, count(*) as runs,
       round(avg(requests),1) as requests, round(avg(tool_calls),1) as tools,
       round(avg(cost_usd),3) as cost, round(avg(peak_context)) as peak_ctx,
       round(avg(cards),2) as cards
from r group by arm;
SQL
echo >&2
echo "== mean per prompt and arm ==" >&2
sqlite3 -header -column :memory: <<SQL >&2
.mode csv
.import '$CSV' r
.mode column
select prompt_id, arm, count(*) as runs, round(avg(requests),1) as requests,
       round(avg(tool_calls),1) as tools, round(avg(cost_usd),3) as cost,
       round(avg(peak_context)) as peak_ctx, round(avg(cards),2) as cards
from r group by prompt_id, arm order by prompt_id, arm;
SQL
echo >&2
echo "results: $CSV   answers: $OUT/*.log" >&2
