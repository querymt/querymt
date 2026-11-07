# Shared helpers for the spec-scored schema scripts. Source, do not execute.
# Requires: POSIX sh, awk, git.

SS_NUM_RE='^[-+]?[0-9]*\.?[0-9]+([eE][-+]?[0-9]+)?$'

# Repository root, physical path (falls back to the current directory outside git).
ss_root() {
  r=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
  (cd "$r" 2>/dev/null && pwd -P) || printf '%s\n' "$r"
}

# Per-repo private state dir (stderr captures, stop overrides). Never committed.
ss_state_dir() {
  d=$(git rev-parse --git-dir 2>/dev/null || echo .git)
  mkdir -p "$d/spec-scored" 2>/dev/null
  echo "$d/spec-scored"
}

# ss_get <metadata-file> <key>: value of `x-score.<key>`, unquoted.
# Line-based: `x-score:` at column 0, fields indented as `  key: value`.
ss_get() {
  awk -v want="$2" '
    /^x-score:[ \t]*$/ { inb = 1; next }
    inb && /^[^ \t#]/ { inb = 0 }
    inb {
      line = $0
      sub(/^[ \t]+/, "", line)
      k = line; sub(/:.*/, "", k)
      if (k != want) next
      v = line; sub(/^[^:]*:[ \t]*/, "", v)
      q = substr(v, 1, 1)
      if (q == "\x27" || q == "\"") {
        v = substr(v, 2); n = length(v)
        while (n > 0 && substr(v, n, 1) != q) n--
        v = substr(v, 1, n - 1)
      } else {
        sub(/[ \t]+#.*$/, "", v); sub(/[ \t]+$/, "", v)
      }
      print v; exit
    }' "$1" 2>/dev/null
}

# ss_list <flow-list>: one item per line from `["a", 'b', c]`.
ss_list() {
  printf '%s\n' "$1" | sed 's/^[[:space:]]*\[//; s/\][[:space:]]*$//' | tr ',' '\n' |
    sed "s/^[[:space:]]*//; s/[[:space:]]*\$//; s/^[\"']//; s/[\"']\$//" | grep -v '^$'
}

# ss_best <rounds.tsv> <direction>: prints "<best> <best_round> <rounds> <first_epoch>".
ss_best() {
  [ -f "$1" ] || { echo "none 0 0 0"; return; }
  awk -F'\t' -v dir="$2" -v re="$SS_NUM_RE" '
    NR == 1 && $1 == "round" { next }
    { n = $1; if (!fe) fe = $2 }
    $3 ~ re {
      v = $3 + 0
      if (!seen || (dir == "min" ? v < bv : v > bv)) { bv = v; bs = $3; br = $1; seen = 1 }
    }
    END { if (seen) print bs, br, n + 0, fe + 0; else print "none", 0, n + 0, fe + 0 }' "$1"
}

# ss_tasks <tasks.md>: prints "<done> <total>" using OpenSpec checkbox rules.
ss_tasks() {
  [ -f "$1" ] || { echo "0 0"; return; }
  awk '
    /^[ \t]*([-*+]|[0-9]+[.)])[ \t]+\[[^]]*\][ \t]+[^ \t]/ {
      t++
      box = $0; sub(/^[^[]*\[/, "", box); sub(/\].*/, "", box); gsub(/[ \t]/, "", box)
      if (box == "x" || box == "X") d++
    }
    END { print d + 0, t + 0 }' "$1"
}

# ss_reached <best> <target> <direction>: exit 0 when the target is met.
ss_reached() {
  [ -n "$2" ] && [ "$1" != none ] &&
    awk -v b="$1" -v t="$2" -v d="$3" 'BEGIN { exit !(d == "min" ? b <= t : b >= t) }'
}

# ss_is_scored <change-dir>: exit 0 when the change uses this schema and has an x-score block.
ss_is_scored() {
  grep -q '^schema:[[:space:]]*spec-scored[[:space:]]*$' "$1/.openspec.yaml" 2>/dev/null &&
    grep -q '^x-score:' "$1/.openspec.yaml" 2>/dev/null
}

# Active scored change, first match wins:
#   1. $SPEC_SCORED_CHANGE
#   2. the non-archived change with the most recently updated rounds.tsv
#   3. a change that is ready to apply but has not logged a round yet (x-score block
#      and tasks.md present, no rounds.tsv), most recently touched first. This lets the
#      hooks guard a change from the very first turn, before the baseline is scored.
ss_active_change() {
  root=$(ss_root)
  if [ -n "${SPEC_SCORED_CHANGE:-}" ]; then
    [ -f "$root/openspec/changes/$SPEC_SCORED_CHANGE/.openspec.yaml" ] && echo "$SPEC_SCORED_CHANGE"
    return
  fi
  found=$(ls -t "$root"/openspec/changes/*/rounds.tsv 2>/dev/null | while read -r f; do
    d=$(dirname "$f")
    if ss_is_scored "$d"; then basename "$d"; break; fi
  done)
  if [ -n "$found" ]; then echo "$found"; return; fi
  ls -t "$root"/openspec/changes/*/.openspec.yaml 2>/dev/null | while read -r f; do
    d=$(dirname "$f")
    [ -e "$d/rounds.tsv" ] && continue
    [ -s "$d/tasks.md" ] || continue
    if ss_is_scored "$d"; then basename "$d"; break; fi
  done
}

# ss_rounds_logged <rounds.tsv>: number of data rows (the baseline counts as one).
ss_rounds_logged() {
  [ -f "$1" ] || { echo 0; return; }
  awk -F'\t' 'NR == 1 && $1 == "round" { next } NF { n++ } END { print n + 0 }' "$1"
}

# ss_snapshot <ref> <message>: commit the full working tree (tracked, modified and
# untracked-but-not-ignored files) and point <ref> at it. Uses a throwaway index, so the
# real index, HEAD and working tree are untouched. Prints the commit id, or nothing on failure.
ss_snapshot() {
  idx=$(mktemp "${TMPDIR:-/tmp}/spec-scored-idx.XXXXXX") || return 1
  real=$(git rev-parse --git-path index 2>/dev/null)
  rm -f "$idx"
  [ -f "$real" ] && cp "$real" "$idx" 2>/dev/null
  tree=$(GIT_INDEX_FILE="$idx" sh -c 'git add -A . >/dev/null 2>&1 && git write-tree' 2>/dev/null)
  rm -f "$idx"
  [ -n "$tree" ] || return 1
  parent=$(git rev-parse -q --verify HEAD 2>/dev/null)
  snap=$(GIT_AUTHOR_NAME=spec-scored GIT_AUTHOR_EMAIL=spec-scored@localhost \
         GIT_COMMITTER_NAME=spec-scored GIT_COMMITTER_EMAIL=spec-scored@localhost \
         git commit-tree "$tree" ${parent:+-p "$parent"} -m "$2" 2>/dev/null)
  [ -n "$snap" ] || return 1
  git update-ref "$1" "$snap" 2>/dev/null || return 1
  printf '%s\n' "$snap"
}

# JSON string escape (no newlines expected).
ss_json() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g' | tr '\n\t' '  '
}
