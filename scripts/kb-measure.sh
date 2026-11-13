#!/usr/bin/env bash
# Repository-knowledge A/B measurement: the same prompts with the feature on
# and off, repeated, summarised from the sessions databases.
#
# Usage: scripts/kb-measure.sh [-n REPEATS] [-p PROMPTS_FILE] [-o OUT_DIR] [-m MAX_PROMPTS] [-a]
#
#   PROMPTS_FILE  one prompt per line; blank lines and lines starting with # are skipped
#                 (default: scripts/kb-measure-prompts.txt)
#   OUT_DIR       per-run databases, logs and results.csv (default: /tmp/kb-measure)
#   -a            allow runs that were priced as API spend (see below)
#
# Credentials. Examples are built with `--features oauth`, and every session
# runs without ANTHROPIC_API_KEY, so the provider can only authenticate with
# the stored OAuth login (`qmt auth login anthropic`). A missing login fails
# the run loudly instead of silently billing an API account. If a run still
# comes back priced in dollars (not an OAuth/subscription attempt), the script
# stops unless -a is given.
#
# Every run calls the model. Roughly (prompts x repeats x 2) sessions.
#
# Each run uses its own sessions database (QMT_SESSIONS_DB), so metrics need no
# session lookup. The index cache is warmed once before the first run, so this
# measures the warm-cache case; delete ~/.qmt/cache/knowledge to test cold.
# Runs alternate between arms (on, off, on, off...) so drift in provider
# latency or cache state does not favour one arm.
#
# Compare arms on tokens: with OAuth there is no dollar cost. This measures
# requests, tool calls and tokens, not answer quality. Final answers are kept
# in OUT_DIR/*.log for reading side by side.
set -euo pipefail

REPEATS=3
MAX_PROMPTS=0
OUT=/tmp/kb-measure
ALLOW_PRICED=0
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROMPTS="$ROOT/scripts/kb-measure-prompts.txt"
while getopts "n:p:o:m:a" opt; do
  case $opt in
    n) REPEATS=$OPTARG ;;
    p) PROMPTS=$OPTARG ;;
    o) OUT=$OPTARG ;;
    m) MAX_PROMPTS=$OPTARG ;;
    a) ALLOW_PRICED=1 ;;
    *) sed -n '2,32p' "$0"; exit 2 ;;
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
OUT="$(cd "$OUT" && pwd)"
CSV="$OUT/results.csv"
echo "prompt_id,arm,rep,requests,tool_calls,in_tokens,out_tokens,cache_read,cache_write,total_tokens,peak_context,cost_usd,cards" > "$CSV"

cd "$ROOT"
echo "building (with OAuth support)..." >&2
cargo build -q -p querymt-agent --features oauth --example from_config --example repository_knowledge 2>/dev/null \
  || { echo "build failed; run: cargo build -p querymt-agent --features oauth --example from_config" >&2; exit 1; }
BIN="$ROOT/target/debug/examples/from_config"
echo "warming the index cache..." >&2
"$ROOT/target/debug/examples/repository_knowledge" --root . refresh >/dev/null 2>&1 || true

# No `mapfile`: macOS ships bash 3.2.
LINES=()
while IFS= read -r line; do LINES+=("$line"); done < <(grep -v -E '^\s*(#|$)' "$PROMPTS")
if [ "$MAX_PROMPTS" -gt 0 ]; then LINES=("${LINES[@]:0:$MAX_PROMPTS}"); fi

# The run has exited, so the database is quiescent. `immutable=1` opens a WAL
# database read-only without needing its -shm file (`mode=ro` fails on it).
metric() { sqlite3 "file:$1?immutable=1" "$2" 2>/dev/null || echo ""; }
usage_sum() { # db field
  metric "$1" "select ifnull(sum(json_extract(payload_json,'\$.data.usage.$2')),0) from event_journal where kind='llm_request_end'"
}

die() { echo "ABORT: $*" >&2; exit 1; }

run_one() { # id arm rep prompt
  local id=$1 arm=$2 rep=$3 prompt=$4 conf db log
  conf=$ON_CONF; [ "$arm" = off ] && conf=$OFF_CONF
  db="$OUT/$id-$arm-$rep.db"; log="$OUT/$id-$arm-$rep.log"
  rm -rf "$db" "$db-wal" "$db-shm" "${db%.db}.run-leases"
  # No API key in the environment: OAuth or nothing.
  printf '%s\nquit\n' "$prompt" \
    | env -u ANTHROPIC_API_KEY QMT_SESSIONS_DB="$db" "$BIN" "${conf#$ROOT/}" >"$log" 2>&1 || true

  local req tools in_t out_t cr cw ctx cost cards total
  req=$(metric "$db" "select count(*) from event_journal where kind='llm_request_end'")
  req=${req:-0}
  # A failed run is never recorded as data.
  if grep -qE '^(Error:|\[ERROR\])' "$log"; then
    die "run $id/$arm/$rep reported an error (see $log): $(grep -m1 -E '^(Error:|\[ERROR\])' "$log" | cut -c1-200)"
  fi
  [ "$req" -gt 0 ] || die "run $id/$arm/$rep made no model request (see $log)"

  tools=$(metric "$db" "select count(*) from event_journal where kind='tool_call_end'")
  in_t=$(usage_sum "$db" input_tokens)
  out_t=$(usage_sum "$db" output_tokens)
  cr=$(usage_sum "$db" cache_read)
  cw=$(usage_sum "$db" cache_write)
  total=$((${in_t:-0} + ${out_t:-0} + ${cr:-0} + ${cw:-0}))
  ctx=$(metric "$db" "select ifnull(max(json_extract(payload_json,'\$.data.context_tokens')),0) from event_journal where kind='llm_request_end'")
  cost=$(metric "$db" "select ifnull(max(json_extract(payload_json,'\$.data.cumulative_cost_usd')),0) from event_journal where kind='llm_request_end'")
  cards=$(metric "$db" "select count(*) from message_parts where part_type='hook_context' and content_json like '%repository_knowledge%'")
  echo "$id,$arm,$rep,$req,${tools:-0},${in_t:-0},${out_t:-0},${cr:-0},${cw:-0},$total,${ctx:-0},${cost:-0},${cards:-0}" | tee -a "$CSV" >&2

  # A dollar price means this was metered API spend, not a subscription attempt.
  if [ "$ALLOW_PRICED" -eq 0 ] && [ "$(echo "${cost:-0} > 0" | bc -l)" = 1 ]; then
    die "run $id/$arm/$rep was priced at \$$cost: it used an API key, not the OAuth login. Run 'qmt auth login anthropic', or pass -a to accept API billing."
  fi
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

summary() { # group-by columns
  sqlite3 -header -column :memory: <<SQL >&2
.mode csv
.import '$CSV' r
.mode column
select $1, count(*) as runs,
       round(avg(requests),1) as requests, round(avg(tool_calls),1) as tools,
       round(avg(in_tokens)) as in_tok, round(avg(out_tokens)) as out_tok,
       round(avg(cache_read)) as cache_rd, round(avg(cache_write)) as cache_wr,
       round(avg(total_tokens)) as total_tok, round(avg(peak_context)) as peak_ctx,
       round(avg(cards),2) as cards
from r group by $1 order by $1;
SQL
}
echo >&2
echo "== mean per arm (all prompts) ==" >&2
summary "arm"
echo >&2
echo "== mean per prompt and arm ==" >&2
summary "prompt_id, arm"
echo >&2
echo "results: $CSV   answers: $OUT/*.log" >&2
