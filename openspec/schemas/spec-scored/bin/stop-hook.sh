#!/bin/sh
# QueryMT `stop` hook for spec-scored changes: keep the turn going while the
# active change still has budget and is not finished.
#
# Finished = all tasks ticked, target reached (or none set), and results.md written.
# Allows the stop when: no active scored change, budget spent, or the override
# file "$(git rev-parse --git-dir)/spec-scored/<change>.allow_stop" exists.
# Always exits 0 (exit 2 is a hard block; any other code fails the turn).
cat > /dev/null
. "$(dirname "$0")/lib.sh"

allow() { printf '{}\n'; exit 0; }
block() {
  printf '{"decision":"block","reason":"%s","hook_specific_output":{"hook_event_name":"stop","additional_context":"%s"}}\n' \
    "$(ss_json "$1")" "$(ss_json "$2")"
  exit 0
}

change=$(ss_active_change)
[ -n "$change" ] || allow
root=$(ss_root); cd "$root" || allow
dir="openspec/changes/$change"; meta="$dir/.openspec.yaml"
[ -f "$(ss_state_dir)/$change.allow_stop" ] && allow

direction=$(ss_get "$meta" direction); direction=${direction:-max}
budget=$(ss_get "$meta" budget_min); budget=${budget:-120}
target=$(ss_get "$meta" target)
stall=$(ss_get "$meta" stall_rounds); stall=${stall:-4}
scorer="sh openspec/schemas/spec-scored/bin/score.sh $change"

set -- $(ss_best "$dir/rounds.tsv" "$direction")
best=$1; best_round=$2; rounds=$3; first=$4
[ "$rounds" -gt 0 ] || block "Change $change has no scored round yet." "Run: $scorer \"baseline\""

elapsed=$(( ($(date +%s) - first) / 60 )); remaining=$(( budget - elapsed ))
[ "$remaining" -le 0 ] && allow

set -- $(ss_tasks "$dir/tasks.md"); done_tasks=$1; total_tasks=$2
reason="Change $change: ${remaining}m of ${budget}m budget left, tasks $done_tasks/$total_tasks, best $best (round $best_round of $rounds)."

if [ "$done_tasks" -lt "$total_tasks" ]; then
  block "$reason" "Continue with the next pending task in $dir/tasks.md, then run: $scorer \"<note>\" and tick the task."
fi
if [ -n "$target" ] && ! ss_reached "$best" "$target" "$direction"; then
  since=$(( rounds - best_round ))
  if [ "$since" -ge "$stall" ]; then
    block "$reason Target $target not reached." "No new best in $since rounds. Restore the best state (git checkout refs/spec-scored/$change/best -- . ':(exclude)$dir') and try a substantially different approach."
  fi
  block "$reason Target $target not reached." "Form one hypothesis, make one focused change, run: $scorer \"<note>\", keep or revert on the result."
fi
if [ ! -s "$dir/results.md" ]; then
  block "$reason Objective met." "Run: $scorer \"final\", write $dir/results.md, then run: openspec validate $change --strict"
fi
allow
