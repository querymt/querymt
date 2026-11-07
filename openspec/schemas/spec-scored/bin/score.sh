#!/bin/sh
# score.sh <change> ["note"] - run the change's x-score command and record a round.
#
# Reads x-score from openspec/changes/<change>/.openspec.yaml, appends a row to
# openspec/changes/<change>/rounds.tsv, snapshots every new best as
# refs/spec-scored/<change>/best, and prints score, best, budget and task progress.
# Exit 0 = scored, 1 = metric failed (the round still counts), 2 = usage/config error.
set -u
. "$(dirname "$0")/lib.sh"

change="${1:-}"
note=$(printf '%s' "${2:-}" | tr '\t\n' '  ')
[ -n "$change" ] || { echo "usage: score.sh <change> [note]" >&2; exit 2; }

root=$(ss_root); cd "$root" || exit 2
dir="openspec/changes/$change"
meta="$dir/.openspec.yaml"
[ -f "$meta" ] || { echo "no change metadata at $meta" >&2; exit 2; }

cmd=$(ss_get "$meta" command)
direction=$(ss_get "$meta" direction); direction=${direction:-max}
budget=$(ss_get "$meta" budget_min); budget=${budget:-120}
target=$(ss_get "$meta" target)
[ -n "$cmd" ] || { echo "x-score.command is missing in $meta (see objective.md)" >&2; exit 2; }
case "$direction" in max|min) ;; *) echo "x-score.direction must be max or min" >&2; exit 2 ;; esac

log="$dir/rounds.tsv"
[ -s "$log" ] || printf 'round\tepoch\tscore\tcommit\tnote\n' > "$log"

state=$(ss_state_dir)
out=$(sh -c "$cmd" 2>"$state/$change.stderr"); rc=$?
score=$(printf '%s\n' "$out" | tail -n1 | tr -d '[:space:]')
if [ "$rc" -ne 0 ] || ! printf '%s' "$score" | awk -v re="$SS_NUM_RE" '{ exit !($0 ~ re) }'; then
  score=FAIL
fi

now=$(date +%s)
set -- $(ss_best "$log" "$direction"); prev_rounds=$3
round=$((prev_rounds + 1))
commit=$(git rev-parse --short HEAD 2>/dev/null || echo -)
printf '%s\t%s\t%s\t%s\t%s\n' "$round" "$now" "$score" "$commit" "$note" >> "$log"

set -- $(ss_best "$log" "$direction")
best=$1; best_round=$2; first=$4
elapsed=$(( (now - first) / 60 )); remaining=$(( budget - elapsed ))
set -- $(ss_tasks "$dir/tasks.md"); done_tasks=$1; total_tasks=$2

ref="refs/spec-scored/$change/best"
status="no improvement"
[ "$round" -eq 1 ] && status="baseline"
if [ "$best_round" = "$round" ]; then
  [ "$round" -gt 1 ] && status="NEW BEST"
  snap=$(git stash create 2>/dev/null)
  [ -n "$snap" ] || snap=$(git rev-parse HEAD 2>/dev/null)
  [ -n "$snap" ] && git update-ref "$ref" "$snap" 2>/dev/null
fi

if [ "$score" = FAIL ]; then
  echo "round $round: FAILED (exit $rc, last stdout line: '$(printf '%s\n' "$out" | tail -n1)'). stderr tail:"
  tail -n 20 "$state/$change.stderr"
fi
echo "round $round: score=$score | $status | best=$best (round $best_round) | budget: ${elapsed}m of ${budget}m used, ${remaining}m left"
if [ -n "$target" ]; then
  if ss_reached "$best" "$target" "$direction"; then t="reached"; else t="not reached"; fi
  echo "target: $target ($direction) - $t"
fi
echo "tasks: $done_tasks/$total_tasks done"
echo "restore best: git checkout $ref -- . ':(exclude)$dir'   (git add new files before scoring so snapshots include them)"
[ "$score" = FAIL ] && exit 1
exit 0
