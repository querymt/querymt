#!/bin/sh
# QueryMT `pre_tool_use` hook for spec-scored changes: deny writes to the scorer,
# the active change's rounds.tsv and .openspec.yaml, and x-score.protected paths.
#
# File tools are checked by their path arguments. `shell` is checked heuristically:
# it is denied when the command names a protected path AND contains a write
# operation (redirect, tee, sed -i, rm, mv, cp, truncate, chmod, git checkout/restore).
# Running the scorer itself is allowed. Requires jq. Always exits 0.
input=$(cat)
. "$(dirname "$0")/lib.sh"

allow() { printf '{}\n'; exit 0; }
deny() {
  printf '{"hook_specific_output":{"hook_event_name":"pre_tool_use","permission_decision":"deny","permission_decision_reason":"%s"}}\n' \
    "$(ss_json "$1")"
  exit 0
}

command -v jq >/dev/null 2>&1 || allow
change=$(ss_active_change)
[ -n "$change" ] || allow
root=$(ss_root)
dir="openspec/changes/$change"

patterns=$(
  echo "openspec/schemas/spec-scored/bin/*"
  echo "$dir/rounds.tsv"
  echo "$dir/.openspec.yaml"
  ss_list "$(ss_get "$root/$dir/.openspec.yaml" protected)"
)

tool=$(printf '%s' "$input" | jq -r '.tool_name // ""')
cwd=$(printf '%s' "$input" | jq -r '.cwd // ""'); [ -n "$cwd" ] || cwd=$root

# Repo-relative, normalised form of a tool path.
relpath() {
  case "$1" in /*) p=$1 ;; *) p="$cwd/$1" ;; esac
  p=$(printf '%s\n' "$p" | awk -F/ '{
        n = 0
        for (i = 1; i <= NF; i++) {
          if ($i == "" || $i == ".") continue
          if ($i == "..") { if (n > 0) n--; continue }
          s[++n] = $i
        }
        out = ""; for (i = 1; i <= n; i++) out = out "/" s[i]; print out }')
  # Resolve symlinked parents (e.g. /var -> /private/var on macOS) via the
  # nearest existing ancestor directory.
  d=$(dirname "$p"); rest=$(basename "$p")
  while [ ! -d "$d" ] && [ "$d" != / ]; do rest="$(basename "$d")/$rest"; d=$(dirname "$d"); done
  pd=$(cd "$d" 2>/dev/null && pwd -P) && p="${pd%/}/$rest"
  case "$p" in "$root"/*) printf '%s\n' "${p#"$root"/}" ;; *) printf '%s\n' "$p" ;; esac
}

protected_match() {  # $1 repo-relative path; prints the matching pattern
  printf '%s\n' "$patterns" | while IFS= read -r pat; do
    [ -n "$pat" ] || continue
    # shellcheck disable=SC2254
    case "$1" in $pat) printf '%s\n' "$pat"; break ;; esac
  done
}

case "$tool" in
  shell)
    cmd=$(printf '%s' "$input" | jq -r '.tool_input.command // ""')
    printf '%s' "$cmd" | grep -Eq '(>|\btee\b|sed[[:space:]]+-i|\brm\b|\bmv\b|\bcp\b|\btruncate\b|\bchmod\b|git[[:space:]]+(checkout|restore))' || allow
    printf '%s\n' "$patterns" | while IFS= read -r pat; do
      lit=$(printf '%s' "$pat" | sed 's/\*.*$//')
      [ -n "$lit" ] && case "$cmd" in *"$lit"*) printf '%s\n' "$pat"; break ;; esac
    done | head -n1 | {
      read -r hit
      [ -n "$hit" ] && deny "spec-scored: shell command would modify protected path '$hit' of change $change. Protected files define the score and must not change during apply."
      allow
    }
    ;;
  *)
    paths=$(printf '%s' "$input" | jq -r '
      .tool_input | [ .path?, .file_path?, (.replacements? // [] | .[]? | .path?) ]
      | .[] | select(type == "string")')
    [ -n "$paths" ] || allow
    printf '%s\n' "$paths" | while IFS= read -r p; do
      r=$(relpath "$p"); hit=$(protected_match "$r")
      [ -n "$hit" ] && { printf '%s\t%s\n' "$r" "$hit"; break; }
    done | {
      IFS="$(printf '\t')" read -r r hit
      [ -n "${hit:-}" ] && deny "spec-scored: '$r' is protected by '$hit' for change $change. Protected files define the score and must not change during apply."
      allow
    }
    ;;
esac
