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
# Isolation. The model under test must not be able to disturb the measurement:
#   * it runs in a throwaway `git worktree` of HEAD under OUT_DIR/wt, not in
#     your checkout, so its file reads, git commands and cargo runs touch
#     nothing real (uncommitted changes are therefore NOT measured);
#   * the example binary is copied to OUT_DIR/bin and run from there, so a
#     rebuild cannot change what is measured (a model that runs
#     `cargo nextest` rebuilds examples without OAuth support);
#   * its tool list is read-only: shell, edit, write_file, multiedit,
#     replace_symbol, question and browse are removed. Prompts must be
#     questions, not implementation tasks.
#
# Credentials. The binary is built with `--features oauth` and every session
# runs without ANTHROPIC_API_KEY, so the provider can only authenticate with
# the stored OAuth login (`qmt auth login anthropic`). A missing login fails
# the run loudly instead of silently billing an API account. If a run still
# comes back priced in dollars, the script stops unless -a is given.
#
# Every run calls the model: roughly (prompts x repeats x 2) sessions.
#
# Each run uses its own sessions database (QMT_SESSIONS_DB). The index cache of
# the worktree is warmed once before the first run, so this measures the
# warm-cache case. Runs alternate between arms (on, off, off, on...) so drift
# in provider latency or cache state does not favour one arm.
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
    *) sed -n '2,40p' "$0"; exit 2 ;;
  esac
done

die() { echo "ABORT: $*" >&2; exit 1; }

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
CSV="$OUT/results.csv"
WT="$OUT/wt"
BIN_DIR="$OUT/bin"
BIN="$BIN_DIR/from_config"
RK="$BIN_DIR/repository_knowledge"

cleanup() {
  git -C "$ROOT" worktree remove --force "$WT" >/dev/null 2>&1 || true
  git -C "$ROOT" worktree prune >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "prompt_id,arm,rep,requests,tool_calls,in_tokens,out_tokens,cache_read,cache_write,total_tokens,peak_context,cost_usd,cards" > "$CSV"

cd "$ROOT"
echo "building (with OAuth support)..." >&2
cargo build -q -p querymt-agent --features oauth --example from_config --example repository_knowledge 2>/dev/null \
  || die "build failed; run: cargo build -p querymt-agent --features oauth --example from_config"

# Pin the binaries: nothing the model does can replace these copies.
rm -rf "$BIN_DIR"; mkdir -p "$BIN_DIR"
cp "$ROOT/target/debug/examples/from_config" "$BIN"
cp "$ROOT/target/debug/examples/repository_knowledge" "$RK"
chmod a-w "$BIN" "$RK"
grep -qa 'OAuth unavailable for' "$BIN" || die "the pinned binary has no OAuth support"

# Throwaway checkout of HEAD for the model to work in.
cleanup
rm -rf "$WT"
git worktree add -q --detach "$WT" HEAD
[ -f "$WT/.qmt/knowledge.toml" ] || die "HEAD has no .qmt/knowledge.toml"

# The config the model runs with. Built from your working-tree example, then:
# arm switch, cwd pinned to the worktree, mutating/interactive tools removed.
CONF_DIR="$WT/crates/agent/examples/confs"
BASE_CONF="$ROOT/crates/agent/examples/confs/repository_knowledge_coder.toml"
FORBIDDEN='shell|edit|write_file|multiedit|replace_symbol|question|browse'
gen_conf() { # arm out
  awk -v arm="$1" -v wt="$WT" -v forbidden="\"($FORBIDDEN)\"" '
    /^cwd = /                 { next }
    /^\[agent\]$/             { print; print "cwd = \"" wt "\""; next }
    /^enabled = (true|false)/ { print "enabled = " (arm == "on" ? "true" : "false"); next }
    /^[ \t]*"/ {
      line = $0
      gsub(forbidden ",? ?", "", line)
      if (line ~ /^[ \t]*$/) next
      print line; next
    }
    { print }
  ' "$BASE_CONF" > "$2"
}
ON_CONF="$CONF_DIR/kb-measure-on.toml"
OFF_CONF="$CONF_DIR/kb-measure-off.toml"
gen_conf on "$ON_CONF"
gen_conf off "$OFF_CONF"
grep -q '^enabled = true$' "$ON_CONF" || die "no 'enabled =' line in $BASE_CONF"
grep -q '^enabled = false$' "$OFF_CONF" || die "arm switch failed"
if grep -E "\"($FORBIDDEN)\"" "$ON_CONF" "$OFF_CONF" >/dev/null; then
  die "a mutating tool is still listed in the generated config"
fi
grep -q '"repo_knowledge_get"' "$ON_CONF" || die "the repository tools are missing from the config"

echo "warming the index cache of the worktree..." >&2
"$RK" --root "$WT" refresh >/dev/null 2>&1 || die "index refresh of the worktree failed"

# No `mapfile`: macOS ships bash 3.2.
LINES=()
while IFS= read -r line; do LINES+=("$line"); done < <(grep -v -E '^\s*(#|$)' "$PROMPTS")
if [ "$MAX_PROMPTS" -gt 0 ]; then LINES=("${LINES[@]:0:$MAX_PROMPTS}"); fi
[ "${#LINES[@]}" -gt 0 ] || die "no prompts in $PROMPTS"

# The run has exited, so the database is quiescent. `immutable=1` opens a WAL
# database read-only without needing its -shm file (`mode=ro` fails on it).
metric() { sqlite3 "file:$1?immutable=1" "$2" 2>/dev/null || echo ""; }
usage_sum() { # db field
  metric "$1" "select ifnull(sum(json_extract(payload_json,'\$.data.usage.$2')),0) from event_journal where kind='llm_request_end'"
}

run_one() { # id arm rep prompt
  local id=$1 arm=$2 rep=$3 prompt=$4 conf db log
  conf=$ON_CONF; [ "$arm" = off ] && conf=$OFF_CONF
  db="$OUT/$id-$arm-$rep.db"; log="$OUT/$id-$arm-$rep.log"
  rm -rf "$db" "$db-wal" "$db-shm" "${db%.db}.run-leases"
  # No API key in the environment: OAuth or nothing. Run inside the worktree.
  ( cd "$WT" && printf '%s\nquit\n' "$prompt" \
      | env -u ANTHROPIC_API_KEY QMT_SESSIONS_DB="$db" "$BIN" "$conf" >"$log" 2>&1 ) || true

  # A failed run is never recorded as data.
  if grep -qE '^(Error:|\[ERROR\])' "$log"; then
    die "run $id/$arm/$rep reported an error (see $log): $(grep -m1 -E '^(Error:|\[ERROR\])' "$log" | cut -c1-200)"
  fi
  local req tools in_t out_t cr cw ctx cost cards total
  req=$(metric "$db" "select count(*) from event_journal where kind='llm_request_end'")
  req=${req:-0}
  [ "$req" -gt 0 ] || die "run $id/$arm/$rep made no model request (see $log)"
  grep -q '^Agent:' "$log" || die "run $id/$arm/$rep did not finish a turn (see $log)"

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
